//! What a publishing RTMP connection does, with the socket left out.
//!
//! Everything below this reads bytes; this decides what they mean. Messages
//! go in, and what to do about them comes back — replies to write, a stream
//! to publish, units to fan out. It holds no socket, spawns nothing and
//! waits for nothing, so a whole publish can be driven in a test with no
//! runtime at all, and so the driver above can be swapped without any of
//! this changing.
//!
//! # The exchange
//!
//! ```text
//! connect("live")        ──▶  window size, peer bandwidth, chunk size,
//!                             _result(NetConnection.Connect.Success)
//! createStream()         ──▶  _result(1)
//! publish("cam1")        ──▶  Stream Begin, onStatus(Publish.Start)
//! @setDataFrame          ──▶  (read past)
//! video sequence header  ──▶  (kept)
//! audio sequence header  ──▶  (kept)
//! first frame            ──▶  Publish { path, description }, then units
//! ```
//!
//! # Why the description waits for a frame
//!
//! A publisher states its tracks one sequence header at a time and never
//! says how many there will be. Waiting for both would hang on a stream that
//! has no sound; publishing at the first would give a silent description to
//! a stream about to send some. So the description is settled at the first
//! frame of actual media: by then a publisher has sent every sequence header
//! it is going to, because a decoder could not read the frame otherwise.
//!
//! Video is track 0 where there is video, whichever header arrived first.

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};

use crate::codec::{aac, h264};
use crate::rtmp::amf0::{self, Amf0Error, Value};
use crate::rtmp::chunk::{ChunkStreamId, Message, MessageType};
use crate::rtmp::flv::{self, AudioTag, FlvError, VideoTag};
use crate::track::{Codec, Description, DescriptionError, TrackId};
use crate::unit::{AudioPayload, AudioUnit, Unit, VideoPayload, VideoUnit};

/// How many bytes may arrive between acknowledgements. A window this size is
/// what every implementation announces; the number itself does not matter,
/// only that both ends agree one is owed.
const WINDOW_SIZE: u32 = 2_500_000;

/// The size this chunks what it sends. Larger than the 128 a connection
/// opens at, because the default costs a header every 128 bytes and this
/// side has no small messages to interleave around.
const CHUNK_SIZE: usize = 65_536;

/// The message stream a published track arrives on. Handed out by
/// `createStream`, and 1 because there is only ever one.
const PUBLISH_STREAM: u32 = 1;

/// What the session wants done.
///
/// Order matters: a [`Action::SetChunkSize`] follows the message that
/// announces it, because the peer goes on reading at the old size until that
/// message reaches it.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Write this message on this chunk stream.
    Send {
        csid: ChunkStreamId,
        message: Box<Message>,
    },

    /// Chunk what is written after this at the new size.
    SetChunkSize(usize),

    /// A publisher has said what it is sending. Nothing arrives before this
    /// and everything after belongs to it.
    Publish {
        path: String,
        description: Box<Description>,
    },

    /// One access unit of the published stream.
    Unit(Box<Unit>),

    /// The publisher is done. Any reader on this path has lost its source.
    Unpublish,
}

/// Why a connection cannot go on.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SessionError {
    #[error(transparent)]
    Amf0(#[from] Amf0Error),

    #[error(transparent)]
    Flv(#[from] FlvError),

    #[error(transparent)]
    H264(#[from] h264::H264Error),

    #[error(transparent)]
    Aac(#[from] aac::AacError),

    #[error(transparent)]
    Description(#[from] DescriptionError),

    /// A command message whose first value is not a command name.
    #[error("a command message with no name")]
    Nameless,

    /// `connect` without the `app` its command object is defined to carry.
    /// There is nowhere to publish to without it.
    #[error("connect named no app")]
    NoApp,

    /// `publish` without a stream key, which is the rest of the path.
    #[error("publish named no stream")]
    NoStream,

    /// Media before `publish`, which is media with nowhere to go.
    #[error("a {0} message arrived before publish")]
    NotPublishing(&'static str),

    /// Media before the sequence header that says how to read it. A video
    /// frame cannot even be split into NAL units without the prefix width
    /// the header declares.
    #[error("a {0} frame arrived before the sequence header that describes it")]
    NoParameters(&'static str),

    /// A second sequence header, saying something different from the first.
    ///
    /// A description is fixed for the life of a stream — see
    /// [`crate::track`] — so this is a publisher starting a different one.
    /// The connection ends and the publisher may open another.
    #[error("the {0} parameters changed mid-stream")]
    ParametersChanged(&'static str),

    /// A message type this does not read. AMF3 and aggregate messages are
    /// carried by nothing that publishes.
    #[error("a message of type {0:?} is one this does not read")]
    UnreadMessage(MessageType),
}

/// What a publisher has stated so far.
#[derive(Debug, Default)]
struct Pending {
    video: Option<h264::AvcConfig>,
    audio: Option<aac::Parameters>,
}

/// A settled stream: what its tracks are, and which id each has.
#[derive(Debug)]
struct Published {
    video: Option<(TrackId, usize)>,
    audio: Option<TrackId>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Before `connect`.
    New,
    /// Connected, and not yet publishing.
    Connected,
    /// `publish` accepted. Media may arrive.
    Publishing,
}

/// One publishing connection's state.
///
/// Fed whole messages, in the order they arrived. See the module docs.
pub struct Session {
    state: State,
    app: String,
    path: String,
    pending: Pending,
    published: Option<Published>,
    timeline: Timeline,
    received: u64,
    acknowledged: u64,
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

impl Session {
    /// A connection that has done nothing but shake hands.
    pub fn new() -> Self {
        Self {
            state: State::New,
            app: String::new(),
            path: String::new(),
            pending: Pending::default(),
            published: None,
            timeline: Timeline::default(),
            received: 0,
            acknowledged: 0,
        }
    }

    /// The path this is publishing to, once `publish` has named one.
    pub fn path(&self) -> Option<&str> {
        (self.state == State::Publishing).then_some(self.path.as_str())
    }

    /// Counts bytes read from the peer, and says when it is owed an
    /// acknowledgement.
    ///
    /// Separate from [`Session::handle`] because the number is the socket's
    /// to know: what has arrived includes chunk headers, and the partial
    /// message the reader is still holding. A publisher that never hears an
    /// acknowledgement will eventually stop sending.
    pub fn received(&mut self, bytes: usize) -> Option<Action> {
        self.received += bytes as u64;
        if self.received - self.acknowledged < u64::from(WINDOW_SIZE) {
            return None;
        }
        self.acknowledged = self.received;
        let mut payload = BytesMut::new();
        // The count is 32 bits and wraps, which both ends expect.
        payload.put_u32(self.received as u32);
        Some(send(
            ChunkStreamId::CONTROL,
            MessageType::Acknowledgement,
            0,
            0,
            payload.freeze(),
        ))
    }

    /// Takes one message and says what to do about it.
    ///
    /// An empty result means the message needed nothing — an
    /// acknowledgement, a chunk size the reader has already applied, a
    /// command this does not answer.
    pub fn handle(&mut self, message: Message) -> Result<Vec<Action>, SessionError> {
        match message.kind {
            MessageType::Amf0Command => self.command(&message),
            MessageType::Video => self.video(&message),
            MessageType::Audio => self.audio(&message),
            // Metadata. Read past rather than read: everything in it about
            // the tracks is also in the sequence headers, stated by the
            // encoder rather than by the muxer, and the two disagree often
            // enough that trusting this one would be a bug.
            MessageType::Amf0Data => Ok(Vec::new()),
            // The chunk layer has already acted on the first two, and the
            // rest are the peer's own housekeeping.
            MessageType::SetChunkSize
            | MessageType::Abort
            | MessageType::Acknowledgement
            | MessageType::WindowAckSize
            | MessageType::SetPeerBandwidth
            | MessageType::UserControl => Ok(Vec::new()),
            kind @ (MessageType::Amf3Command | MessageType::Amf3Data | MessageType::Aggregate) => {
                Err(SessionError::UnreadMessage(kind))
            }
            // A message type nobody defines is a message nobody sent on
            // purpose. Forwarding is not an option and neither is reading
            // it, so it goes by.
            MessageType::Other(_) => Ok(Vec::new()),
        }
    }

    fn command(&mut self, message: &Message) -> Result<Vec<Action>, SessionError> {
        let values = amf0::read_all(&message.payload)?;
        let name = values
            .first()
            .and_then(Value::as_str)
            .ok_or(SessionError::Nameless)?;
        let transaction = values.get(1).and_then(Value::as_f64).unwrap_or(0.0);

        match name {
            "connect" => self.connect(&values, transaction),
            "createStream" => Ok(vec![result(
                transaction,
                vec![Value::Null, Value::Number(f64::from(PUBLISH_STREAM))],
            )]),
            "publish" => self.publish(&values),
            "deleteStream" | "closeStream" | "FCUnpublish" => {
                let was = std::mem::replace(&mut self.state, State::Connected);
                Ok(if was == State::Publishing {
                    vec![Action::Unpublish]
                } else {
                    Vec::new()
                })
            }
            // `releaseStream` and `FCPublish` are what Flash Media Live
            // Encoder asked before publishing, and what everything descended
            // from it still sends. Nothing waits for an answer.
            //
            // Anything else is a call this is not a server for. A client is
            // free to make one; not answering is the answer.
            _ => Ok(Vec::new()),
        }
    }

    fn connect(&mut self, values: &[Value], transaction: f64) -> Result<Vec<Action>, SessionError> {
        self.app = values
            .get(2)
            .and_then(|object| object.get("app"))
            .and_then(Value::as_str)
            .ok_or(SessionError::NoApp)?
            .trim_matches('/')
            .to_owned();
        self.state = State::Connected;

        let mut window = BytesMut::new();
        window.put_u32(WINDOW_SIZE);
        let mut bandwidth = BytesMut::new();
        bandwidth.put_u32(WINDOW_SIZE);
        // Limit type 2, "dynamic": the peer may use the window as a hard or
        // a soft limit as it sees fit. Nothing does anything else.
        bandwidth.put_u8(2);
        let mut chunk_size = BytesMut::new();
        chunk_size.put_u32(CHUNK_SIZE as u32);

        Ok(vec![
            send(
                ChunkStreamId::CONTROL,
                MessageType::WindowAckSize,
                0,
                0,
                window.freeze(),
            ),
            send(
                ChunkStreamId::CONTROL,
                MessageType::SetPeerBandwidth,
                0,
                0,
                bandwidth.freeze(),
            ),
            send(
                ChunkStreamId::CONTROL,
                MessageType::SetChunkSize,
                0,
                0,
                chunk_size.freeze(),
            ),
            // After the message that announces it, never before.
            Action::SetChunkSize(CHUNK_SIZE),
            result(
                transaction,
                vec![
                    Value::Object(vec![
                        (
                            "fmsVer".to_owned(),
                            Value::String("FMS/3,0,1,123".to_owned()),
                        ),
                        ("capabilities".to_owned(), Value::Number(31.0)),
                    ]),
                    status(
                        "NetConnection.Connect.Success",
                        "Connection succeeded.",
                        &[("objectEncoding", Value::Number(0.0))],
                    ),
                ],
            ),
        ])
    }

    fn publish(&mut self, values: &[Value]) -> Result<Vec<Action>, SessionError> {
        // The command object at index 2 is null for `publish`; the stream
        // key is next, and the publishing type after that.
        let key = values
            .get(3)
            .and_then(Value::as_str)
            .ok_or(SessionError::NoStream)?;
        // Everything from the first `?` is the encoder's query string, where
        // a token goes. It is not part of the name, and leaving it in would
        // put the stream somewhere no reader thinks to look.
        let key = key.split('?').next().unwrap_or(key).trim_matches('/');
        if key.is_empty() {
            return Err(SessionError::NoStream);
        }
        self.path = format!("{}/{key}", self.app);
        self.state = State::Publishing;

        let mut begin = BytesMut::new();
        // User control event 0, "Stream Begin".
        begin.put_u16(0);
        begin.put_u32(PUBLISH_STREAM);

        Ok(vec![
            send(
                ChunkStreamId::CONTROL,
                MessageType::UserControl,
                0,
                0,
                begin.freeze(),
            ),
            on_status(status(
                "NetStream.Publish.Start",
                "Publishing.",
                &[("details", Value::String(key.to_owned()))],
            )),
        ])
    }

    fn video(&mut self, message: &Message) -> Result<Vec<Action>, SessionError> {
        if self.state != State::Publishing {
            return Err(SessionError::NotPublishing("video"));
        }
        match flv::read_video(&message.payload)? {
            VideoTag::SequenceHeader(record) => {
                let config = h264::AvcConfig::parse(&record)?;
                self.state(Kind::Video, |pending| &mut pending.video, config)
            }
            VideoTag::Picture {
                composition_time,
                data,
                ..
            } => {
                let mut actions = self.settle()?;
                let published = self.published.as_ref().expect("settled");
                let Some((track, nal_length_size)) = published.video else {
                    // Video after a description settled without it, which
                    // means the first frame was sound and the publisher only
                    // then began sending pictures. It is a different stream.
                    return Err(SessionError::ParametersChanged("video"));
                };
                let dts = self.timeline.at(message.timestamp);
                let nalus = h264::split_length_prefixed(&data, nal_length_size)?;
                actions.push(Action::Unit(Box::new(Unit::Video(VideoUnit::new(
                    track,
                    VideoPayload::H264(nalus),
                    offset(dts, composition_time),
                    dts,
                )))));
                Ok(actions)
            }
            // The publisher says there will be no more pictures, without
            // saying the stream is over. Nothing to do: the track stays
            // described and simply goes quiet.
            VideoTag::End => Ok(Vec::new()),
        }
    }

    fn audio(&mut self, message: &Message) -> Result<Vec<Action>, SessionError> {
        if self.state != State::Publishing {
            return Err(SessionError::NotPublishing("audio"));
        }
        match flv::read_audio(&message.payload)? {
            AudioTag::SequenceHeader(record) => {
                let parameters = aac::Parameters::parse(record)?;
                self.state(Kind::Audio, |pending| &mut pending.audio, parameters)
            }
            AudioTag::Frame(frame) => {
                let mut actions = self.settle()?;
                let published = self.published.as_ref().expect("settled");
                let Some(track) = published.audio else {
                    return Err(SessionError::ParametersChanged("audio"));
                };
                let pts = self.timeline.at(message.timestamp);
                actions.push(Action::Unit(Box::new(Unit::Audio(AudioUnit {
                    track,
                    payload: AudioPayload::Aac(frame),
                    pts,
                }))));
                Ok(actions)
            }
        }
    }

    /// Takes a sequence header, refusing one that contradicts what a
    /// description was already built on.
    fn state<T: PartialEq>(
        &mut self,
        kind: Kind,
        field: impl Fn(&mut Pending) -> &mut Option<T>,
        stated: T,
    ) -> Result<Vec<Action>, SessionError> {
        let slot = field(&mut self.pending);
        // A publisher repeating what it already said is common and harmless
        // — some send the headers again before every keyframe.
        if slot.as_ref() == Some(&stated) {
            return Ok(Vec::new());
        }
        if self.published.is_some() || slot.is_some() {
            return Err(SessionError::ParametersChanged(kind.name()));
        }
        *slot = Some(stated);
        Ok(Vec::new())
    }

    /// Settles the description if it is not settled yet. See the module docs
    /// on why this happens at the first frame rather than at a header.
    fn settle(&mut self) -> Result<Vec<Action>, SessionError> {
        if self.published.is_some() {
            return Ok(Vec::new());
        }
        let mut codecs = Vec::new();
        // Video first, so that a stream with pictures has them on track 0
        // whichever header arrived first.
        let video = self.pending.video.as_ref().map(|config| {
            codecs.push(Codec::H264(config.parameters.clone()));
            config.nal_length_size
        });
        let audio = self.pending.audio.as_ref().map(|parameters| {
            codecs.push(Codec::Aac(parameters.clone()));
        });
        if codecs.is_empty() {
            return Err(SessionError::NoParameters("media"));
        }

        let description = Description::new(codecs)?;
        let mut tracks = description.tracks().iter().map(crate::track::Track::id);
        self.published = Some(Published {
            video: video.map(|size| (tracks.next().expect("a video track"), size)),
            audio: audio.map(|()| tracks.next().expect("an audio track")),
        });
        Ok(vec![Action::Publish {
            path: self.path.clone(),
            description: Box::new(description),
        }])
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Video,
    Audio,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
        }
    }
}

/// Turns RTMP's counter into a timeline that starts at zero and goes
/// forwards.
///
/// The counter is 32 bits of milliseconds from an origin the publisher chose
/// and never states, so the first message seen is the origin, and it wraps
/// after 49 days. A stream that ran that long and then went back to zero is
/// not starting again.
#[derive(Debug, Default)]
struct Timeline {
    origin: Option<u32>,
    last: u32,
    wraps: u64,
}

impl Timeline {
    fn at(&mut self, timestamp: u32) -> Duration {
        let origin = *self.origin.get_or_insert(timestamp);
        // A large step backwards is the counter wrapping; a small one is a
        // publisher whose timestamps are out of order, which happens and
        // which nothing can be done about.
        if timestamp < self.last && self.last - timestamp > u32::MAX / 2 {
            self.wraps += 1;
        }
        self.last = timestamp;
        let elapsed = (self.wraps << 32)
            .saturating_add(u64::from(timestamp))
            .saturating_sub(u64::from(origin));
        Duration::from_millis(elapsed)
    }
}

/// A presentation time, which is a decode time plus an offset that is
/// negative wherever there are B-frames.
fn offset(dts: Duration, composition_time: i32) -> Duration {
    let by = Duration::from_millis(composition_time.unsigned_abs().into());
    if composition_time < 0 {
        // Only at the very start of a stream, where the first picture's
        // decode time is zero and its offset points before it. There is no
        // earlier moment to name.
        dts.saturating_sub(by)
    } else {
        dts + by
    }
}

fn send(
    csid: ChunkStreamId,
    kind: MessageType,
    stream_id: u32,
    timestamp: u32,
    payload: Bytes,
) -> Action {
    Action::Send {
        csid,
        message: Box::new(Message {
            timestamp,
            kind,
            stream_id,
            payload,
        }),
    }
}

/// A command's answer, which carries the transaction number it answers.
fn result(transaction: f64, mut values: Vec<Value>) -> Action {
    let mut out = BytesMut::new();
    let mut all = vec![
        Value::String("_result".to_owned()),
        Value::Number(transaction),
    ];
    all.append(&mut values);
    amf0::write_all(&mut out, &all).expect("values this builds are writable");
    send(
        ChunkStreamId::COMMAND,
        MessageType::Amf0Command,
        0,
        0,
        out.freeze(),
    )
}

/// A status told to the publisher rather than asked of it, so it answers no
/// transaction and goes on the stream it is about.
fn on_status(status: Value) -> Action {
    let mut out = BytesMut::new();
    amf0::write_all(
        &mut out,
        &[
            Value::String("onStatus".to_owned()),
            Value::Number(0.0),
            Value::Null,
            status,
        ],
    )
    .expect("values this builds are writable");
    send(
        ChunkStreamId::COMMAND,
        MessageType::Amf0Command,
        PUBLISH_STREAM,
        0,
        out.freeze(),
    )
}

fn status(code: &str, description: &str, extra: &[(&str, Value)]) -> Value {
    let mut properties = vec![
        ("level".to_owned(), Value::String("status".to_owned())),
        ("code".to_owned(), Value::String(code.to_owned())),
        (
            "description".to_owned(),
            Value::String(description.to_owned()),
        ),
    ];
    properties.extend(
        extra
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone())),
    );
    Value::Object(properties)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::track::Kind as TrackKind;

    /// An `AVCDecoderConfigurationRecord` with four-byte length prefixes.
    fn avc_record() -> Bytes {
        let config = h264::AvcConfig {
            parameters: h264::Parameters {
                sps: vec![
                    h264::Nal::new(Bytes::from_static(&[
                        0x67, 0x42, 0xc0, 0x1e, 0xd9, 0x00, 0x80,
                    ]))
                    .unwrap(),
                ],
                pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c, 0x80])).unwrap()],
            },
            nal_length_size: 4,
        };
        config.to_bytes().unwrap()
    }

    fn command(name: &str, transaction: f64, rest: &[Value]) -> Message {
        let mut values = vec![Value::String(name.to_owned()), Value::Number(transaction)];
        values.extend_from_slice(rest);
        let mut payload = BytesMut::new();
        amf0::write_all(&mut payload, &values).unwrap();
        Message {
            timestamp: 0,
            kind: MessageType::Amf0Command,
            stream_id: 0,
            payload: payload.freeze(),
        }
    }

    fn connect(app: &str) -> Message {
        command(
            "connect",
            1.0,
            &[Value::Object(vec![(
                "app".to_owned(),
                Value::String(app.to_owned()),
            )])],
        )
    }

    fn publish(key: &str) -> Message {
        command(
            "publish",
            4.0,
            &[
                Value::Null,
                Value::String(key.to_owned()),
                Value::String("live".to_owned()),
            ],
        )
    }

    fn media(kind: MessageType, timestamp: u32, payload: Bytes) -> Message {
        Message {
            timestamp,
            kind,
            stream_id: PUBLISH_STREAM,
            payload,
        }
    }

    fn video_header() -> Message {
        media(
            MessageType::Video,
            0,
            flv::write_video(&VideoTag::SequenceHeader(avc_record())).unwrap(),
        )
    }

    fn audio_header() -> Message {
        media(
            MessageType::Audio,
            0,
            flv::write_audio(&AudioTag::SequenceHeader(Bytes::from_static(&[0x12, 0x10]))),
        )
    }

    /// One access unit of length-prefixed NAL units: an IDR.
    fn picture(timestamp: u32, composition_time: i32) -> Message {
        let mut data = BytesMut::new();
        data.put_u32(2);
        data.put_slice(&[0x65, 0x88]);
        media(
            MessageType::Video,
            timestamp,
            flv::write_video(&VideoTag::Picture {
                keyframe: true,
                composition_time,
                data: data.freeze(),
            })
            .unwrap(),
        )
    }

    fn frame(timestamp: u32) -> Message {
        media(
            MessageType::Audio,
            timestamp,
            flv::write_audio(&AudioTag::Frame(Bytes::from_static(&[0x21, 0x00]))),
        )
    }

    /// Runs a session up to and including `publish`, discarding the replies.
    fn publishing() -> Session {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        session.handle(command("createStream", 2.0, &[])).unwrap();
        session.handle(publish("cam1")).unwrap();
        session
    }

    fn only_unit(actions: Vec<Action>) -> Unit {
        let [Action::Unit(unit)] = <[Action; 1]>::try_from(actions).expect("one action") else {
            panic!("a unit");
        };
        *unit
    }

    #[test]
    fn connect_is_answered_with_the_four_things_a_client_waits_for() {
        let mut session = Session::new();
        let actions = session.handle(connect("live")).unwrap();

        let kinds: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                Action::Send { message, .. } => Some(message.kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            kinds,
            [
                MessageType::WindowAckSize,
                MessageType::SetPeerBandwidth,
                MessageType::SetChunkSize,
                MessageType::Amf0Command,
            ]
        );

        // The chunk size takes effect after the message announcing it, or
        // the peer reads the announcement itself at the wrong size.
        let announced = actions
            .iter()
            .position(|action| matches!(action, Action::Send { message, .. } if message.kind == MessageType::SetChunkSize));
        let applied = actions
            .iter()
            .position(|action| matches!(action, Action::SetChunkSize(_)));
        assert!(announced < applied);
    }

    #[test]
    fn the_result_carries_the_transaction_it_answers_and_says_it_succeeded() {
        let mut session = Session::new();
        let actions = session.handle(connect("live")).unwrap();
        let Some(Action::Send { message, .. }) = actions.last() else {
            panic!("a reply")
        };
        let values = amf0::read_all(&message.payload).unwrap();
        assert_eq!(values[0].as_str(), Some("_result"));
        assert_eq!(values[1].as_f64(), Some(1.0));
        assert_eq!(
            values[3].get("code").and_then(Value::as_str),
            Some("NetConnection.Connect.Success")
        );
    }

    #[test]
    fn create_stream_hands_out_the_stream_publish_then_arrives_on() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        let actions = session.handle(command("createStream", 2.0, &[])).unwrap();
        let [Action::Send { message, .. }] = &actions[..] else {
            panic!("one reply")
        };
        let values = amf0::read_all(&message.payload).unwrap();
        assert_eq!(values[0].as_str(), Some("_result"));
        assert_eq!(values[1].as_f64(), Some(2.0));
        assert_eq!(values[3].as_f64(), Some(f64::from(PUBLISH_STREAM)));
    }

    #[test]
    fn publish_is_answered_with_stream_begin_and_a_status() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        let actions = session.handle(publish("cam1")).unwrap();
        let [
            Action::Send {
                message: begin,
                csid,
            },
            Action::Send {
                message: status, ..
            },
        ] = &actions[..]
        else {
            panic!("two replies")
        };
        assert_eq!(*csid, ChunkStreamId::CONTROL);
        assert_eq!(begin.kind, MessageType::UserControl);
        // Event 0, on the published stream.
        assert_eq!(&begin.payload[..], &[0, 0, 0, 0, 0, 1]);

        let values = amf0::read_all(&status.payload).unwrap();
        assert_eq!(values[0].as_str(), Some("onStatus"));
        assert_eq!(
            values[3].get("code").and_then(Value::as_str),
            Some("NetStream.Publish.Start")
        );
        assert_eq!(status.stream_id, PUBLISH_STREAM);
    }

    #[test]
    fn a_path_is_the_app_and_the_stream_key() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        session.handle(publish("cam1")).unwrap();
        assert_eq!(session.path(), Some("live/cam1"));
    }

    #[test]
    fn a_stream_key_loses_the_query_an_encoder_puts_on_it() {
        // Where a token goes. Leaving it in would put the stream somewhere
        // no reader thinks to look.
        let mut session = Session::new();
        session.handle(connect("/live/")).unwrap();
        session.handle(publish("cam1?token=abc&x=1")).unwrap();
        assert_eq!(session.path(), Some("live/cam1"));
    }

    #[test]
    fn a_publish_with_no_stream_key_is_refused() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        assert_eq!(
            session.handle(publish("?token=abc")),
            Err(SessionError::NoStream)
        );
    }

    #[test]
    fn a_connect_that_names_no_app_is_refused() {
        let mut session = Session::new();
        assert_eq!(
            session.handle(command("connect", 1.0, &[Value::Object(Vec::new())])),
            Err(SessionError::NoApp)
        );
    }

    #[test]
    fn the_description_settles_at_the_first_frame_and_not_before() {
        let mut session = publishing();
        assert!(session.handle(video_header()).unwrap().is_empty());
        assert!(session.handle(audio_header()).unwrap().is_empty());

        let actions = session.handle(picture(0, 0)).unwrap();
        let [Action::Publish { path, description }, Action::Unit(_)] = &actions[..] else {
            panic!("a publish and then a unit, got {actions:?}")
        };
        assert_eq!(path, "live/cam1");
        assert_eq!(description.tracks().len(), 2);
        assert_eq!(description.tracks()[0].kind(), TrackKind::Video);
        assert_eq!(description.tracks()[1].kind(), TrackKind::Audio);

        // And only once.
        let actions = session.handle(picture(33, 0)).unwrap();
        assert!(matches!(&actions[..], [Action::Unit(_)]));
    }

    #[test]
    fn a_stream_with_no_sound_does_not_wait_for_any() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        let actions = session.handle(picture(0, 0)).unwrap();
        let [Action::Publish { description, .. }, _] = &actions[..] else {
            panic!("a publish")
        };
        assert_eq!(description.tracks().len(), 1);
        assert_eq!(description.tracks()[0].kind(), TrackKind::Video);
    }

    #[test]
    fn a_stream_with_no_pictures_puts_its_sound_on_the_first_track() {
        let mut session = publishing();
        session.handle(audio_header()).unwrap();
        let actions = session.handle(frame(0)).unwrap();
        let [Action::Publish { description, .. }, _] = &actions[..] else {
            panic!("a publish")
        };
        assert_eq!(description.tracks().len(), 1);
        assert_eq!(description.tracks()[0].kind(), TrackKind::Audio);
    }

    #[test]
    fn pictures_arrive_as_unframed_nal_units_on_the_video_track() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(picture(0, 0)).unwrap();

        let unit = only_unit(session.handle(picture(33, 0)).unwrap());
        let Unit::Video(video) = unit else {
            panic!("a picture")
        };
        assert_eq!(video.track.index(), 0);
        assert!(video.keyframe);
        assert_eq!(
            video.payload,
            VideoPayload::H264(vec![
                h264::Nal::new(Bytes::from_static(&[0x65, 0x88])).unwrap()
            ])
        );
    }

    #[test]
    fn a_timeline_starts_where_the_publisher_started() {
        // A publisher whose counter does not begin at zero. A reader still
        // needs a timeline that does.
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(picture(1_000_000, 0)).unwrap();

        let unit = only_unit(session.handle(picture(1_000_033, 0)).unwrap());
        assert_eq!(unit.pts(), Duration::from_millis(33));
    }

    #[test]
    fn a_counter_that_wraps_does_not_send_the_stream_back_to_the_beginning() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(picture(u32::MAX - 100, 0)).unwrap();

        // 100 ms to the wrap, then 33 past it.
        let unit = only_unit(session.handle(picture(32, 0)).unwrap());
        assert_eq!(unit.pts(), Duration::from_millis(133));
    }

    #[test]
    fn a_presentation_time_can_be_ahead_of_a_decode_time() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(picture(0, 0)).unwrap();

        let unit = only_unit(session.handle(picture(100, 66)).unwrap());
        let Unit::Video(video) = unit else {
            panic!("a picture")
        };
        assert_eq!(video.dts, Duration::from_millis(100));
        assert_eq!(video.pts, Duration::from_millis(166));

        // And behind it, which is what a stream with B-frames sends.
        let unit = only_unit(session.handle(picture(200, -30)).unwrap());
        let Unit::Video(video) = unit else {
            panic!("a picture")
        };
        assert_eq!(video.pts, Duration::from_millis(170));
    }

    #[test]
    fn sound_arrives_as_raw_frames_on_its_own_track() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(audio_header()).unwrap();
        session.handle(picture(0, 0)).unwrap();

        let unit = only_unit(session.handle(frame(23)).unwrap());
        let Unit::Audio(audio) = unit else {
            panic!("a frame")
        };
        assert_eq!(audio.track.index(), 1);
        assert_eq!(audio.pts, Duration::from_millis(23));
        assert_eq!(
            audio.payload,
            AudioPayload::Aac(Bytes::from_static(&[0x21, 0x00]))
        );
    }

    #[test]
    fn media_before_publish_is_refused() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        assert_eq!(
            session.handle(video_header()),
            Err(SessionError::NotPublishing("video"))
        );
        assert_eq!(
            session.handle(audio_header()),
            Err(SessionError::NotPublishing("audio"))
        );
    }

    #[test]
    fn a_frame_before_any_sequence_header_is_refused() {
        let mut session = publishing();
        assert_eq!(
            session.handle(picture(0, 0)),
            Err(SessionError::NoParameters("media"))
        );
    }

    #[test]
    fn a_repeated_sequence_header_is_taken_as_the_same_stream() {
        // Some encoders send the headers again before every keyframe.
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        assert!(session.handle(video_header()).unwrap().is_empty());
        session.handle(picture(0, 0)).unwrap();
        assert!(session.handle(video_header()).unwrap().is_empty());
    }

    #[test]
    fn a_sequence_header_that_says_something_else_ends_the_connection() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();

        // The same parameter sets at a different prefix width is still a
        // different stream: every payload after it is framed another way.
        let changed = h264::AvcConfig {
            parameters: h264::AvcConfig::parse(&avc_record()).unwrap().parameters,
            nal_length_size: 2,
        };
        let message = media(
            MessageType::Video,
            0,
            flv::write_video(&VideoTag::SequenceHeader(changed.to_bytes().unwrap())).unwrap(),
        );
        assert_eq!(
            session.handle(message),
            Err(SessionError::ParametersChanged("video"))
        );
    }

    #[test]
    fn sound_that_turns_up_after_the_description_settled_ends_the_connection() {
        let mut session = publishing();
        session.handle(video_header()).unwrap();
        session.handle(picture(0, 0)).unwrap();
        assert_eq!(
            session.handle(audio_header()),
            Err(SessionError::ParametersChanged("audio"))
        );
    }

    #[test]
    fn closing_the_stream_says_the_source_is_gone() {
        let mut session = publishing();
        assert_eq!(
            session.handle(command("deleteStream", 5.0, &[])).unwrap(),
            vec![Action::Unpublish]
        );
        // And again is nothing: there is no source left to lose.
        assert!(
            session
                .handle(command("closeStream", 6.0, &[]))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn the_calls_an_encoder_makes_and_does_not_wait_for_are_let_by() {
        let mut session = Session::new();
        session.handle(connect("live")).unwrap();
        for name in ["releaseStream", "FCPublish", "somethingElse"] {
            assert!(
                session
                    .handle(command(name, 3.0, &[Value::Null]))
                    .unwrap()
                    .is_empty(),
                "{name}"
            );
        }
    }

    #[test]
    fn an_acknowledgement_is_owed_once_a_window_has_arrived() {
        let mut session = Session::new();
        assert_eq!(session.received(WINDOW_SIZE as usize - 1), None);
        let Some(Action::Send { message, csid }) = session.received(1) else {
            panic!("an acknowledgement")
        };
        assert_eq!(csid, ChunkStreamId::CONTROL);
        assert_eq!(message.kind, MessageType::Acknowledgement);
        assert_eq!(&message.payload[..], &WINDOW_SIZE.to_be_bytes());

        // And not again until the next window.
        assert_eq!(session.received(1), None);
    }

    #[test]
    fn a_message_type_this_does_not_read_is_named() {
        let mut session = Session::new();
        let message = Message {
            timestamp: 0,
            kind: MessageType::Amf3Command,
            stream_id: 0,
            payload: Bytes::new(),
        };
        assert_eq!(
            session.handle(message),
            Err(SessionError::UnreadMessage(MessageType::Amf3Command))
        );
    }
}
