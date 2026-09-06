//! RTMP's chunk stream: how a message is cut up to go on the wire, and how
//! the pieces are put back together.
//!
//! # Why there are chunks at all
//!
//! One connection carries audio, video and commands at once. A reader that
//! had to take a 200 kB keyframe in one piece would see nothing else until
//! it finished, so RTMP cuts every message into chunks of at most
//! `chunk_size` bytes and lets chunks of different messages interleave.
//! Reassembling them is this module's whole job.
//!
//! # Why a chunk cannot be read on its own
//!
//! A chunk header repeats what its message already said — when it belongs,
//! how long it is, what it is, which stream it is on — and at thirty
//! pictures a second that is a great deal of repetition. RTMP's answer is
//! four header formats, each leaving out more than the last, with whatever
//! is missing taken from the previous chunk *on the same chunk stream*:
//!
//! | Format | States                          | Its timestamp is |
//! | ------ | ------------------------------- | ---------------- |
//! | 0      | timestamp, length, type, stream | absolute         |
//! | 1      | timestamp, length, type         | a delta          |
//! | 2      | timestamp                       | a delta          |
//! | 3      | nothing                         | inherited        |
//!
//! So the same eleven bytes mean different things depending on what came
//! before them, and a reader has to remember the last chunk of every chunk
//! stream it has seen. That is why [`Reader`] and [`Writer`] hold state, and
//! why one of each belongs to one connection and cannot be shared.
//!
//! # What the reader does for itself
//!
//! [`Reader`] applies `SetChunkSize` and `Abort` as it reads them, rather
//! than handing them up and waiting to be told. Both reconfigure the chunk
//! stream itself — one moves where the next chunk boundary falls, the other
//! throws away a half-read message — and both take effect at the chunk that
//! follows. A caller that queued messages before acting on them would go on
//! parsing at the old size and read rubbish. The messages are still
//! returned, so a caller that wants to log them can; acting on them is not
//! required.
//!
//! Everything else is passed through untouched. `WindowAckSize` and
//! `SetPeerBandwidth` are about the connection rather than about chunks, and
//! belong to whoever owns the connection.

use std::collections::HashMap;

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The chunk size a connection starts at, before either peer has said
/// otherwise. It is small enough that one picture is dozens of chunks, which
/// is why raising it is usually an implementation's first act.
pub const DEFAULT_CHUNK_SIZE: usize = 128;

/// The largest chunk size a peer may ask for. The field is 31 bits wide, but
/// a chunk larger than the messages going through it defeats the
/// interleaving chunks exist for, and the specification says not to go past
/// this.
const MAX_CHUNK_SIZE: usize = 0xff_ffff;

/// The largest message RTMP can express: its length field is three bytes.
const MAX_MESSAGE_LENGTH: usize = 0xff_ffff;

/// The largest message [`Reader`] will assemble, under what the protocol can
/// express. The biggest thing RTMP legitimately carries is a video keyframe,
/// which runs to a couple of megabytes at bitrates nobody sends; past that a
/// declared length is either a mistake or an attempt to make a server hold
/// memory on the strength of one header.
pub const MAX_ASSEMBLED_LENGTH: usize = 8 * 1024 * 1024;

/// How many chunk streams one connection may open. An encoder uses a
/// handful — one for control, one for commands, one each for audio and
/// video — and the identifier space runs to 65 599, every one of which would
/// otherwise get a reassembly buffer.
pub const MAX_CHUNK_STREAMS: usize = 64;

/// What a three-byte timestamp field holds to say the real value follows the
/// header in four bytes.
const EXTENDED_TIMESTAMP: u32 = 0xff_ffff;

/// What a message is, from its type id.
///
/// The first six are RTMP's own housekeeping and travel on message stream 0;
/// the rest are what a session is made of.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageType {
    /// Changes how large the sender's chunks are, from the next one on.
    SetChunkSize,
    /// Throws away the partly-sent message on one chunk stream.
    Abort,
    /// Reports how many bytes the sender has received.
    Acknowledgement,
    /// Stream control: begin, end, buffer length, ping.
    UserControl,
    /// How many bytes the sender will send before it wants an
    /// acknowledgement.
    WindowAckSize,
    /// Asks the peer to hold its window to a size.
    SetPeerBandwidth,
    /// One audio frame, in whatever codec the stream declared.
    Audio,
    /// One coded picture, in whatever codec the stream declared.
    Video,
    /// A metadata message, AMF3-encoded.
    Amf3Data,
    /// A remote call, AMF3-encoded.
    Amf3Command,
    /// A metadata message, AMF0-encoded — `@setDataFrame`, `onMetaData`.
    Amf0Data,
    /// A remote call, AMF0-encoded — `connect`, `createStream`, `publish`.
    Amf0Command,
    /// Several messages packed into one.
    Aggregate,
    /// Anything else, kept verbatim: a relay forwards what it does not
    /// recognize rather than dropping it.
    Other(u8),
}

impl MessageType {
    /// Reads a type id.
    pub fn from_id(id: u8) -> Self {
        match id {
            1 => Self::SetChunkSize,
            2 => Self::Abort,
            3 => Self::Acknowledgement,
            4 => Self::UserControl,
            5 => Self::WindowAckSize,
            6 => Self::SetPeerBandwidth,
            8 => Self::Audio,
            9 => Self::Video,
            15 => Self::Amf3Data,
            17 => Self::Amf3Command,
            18 => Self::Amf0Data,
            20 => Self::Amf0Command,
            22 => Self::Aggregate,
            other => Self::Other(other),
        }
    }

    /// The type id this stands for.
    pub fn id(self) -> u8 {
        match self {
            Self::SetChunkSize => 1,
            Self::Abort => 2,
            Self::Acknowledgement => 3,
            Self::UserControl => 4,
            Self::WindowAckSize => 5,
            Self::SetPeerBandwidth => 6,
            Self::Audio => 8,
            Self::Video => 9,
            Self::Amf3Data => 15,
            Self::Amf3Command => 17,
            Self::Amf0Data => 18,
            Self::Amf0Command => 20,
            Self::Aggregate => 22,
            Self::Other(id) => id,
        }
    }
}

/// One whole message, with the chunking undone.
///
/// It carries no chunk stream id. Which chunk stream a message arrived on is
/// a fact about the wire rather than about the message — a peer may move one
/// kind of message between chunk streams freely — so it ends at this
/// module's edge. [`Writer::write`] takes the one to send on as an argument
/// for the same reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// When the message belongs, in milliseconds from an origin the peer
    /// chose and never states.
    ///
    /// Kept as the protocol's own 32-bit counter, which wraps, rather than
    /// as a [`std::time::Duration`]: turning it into a timeline that starts
    /// at zero means knowing which message came first, and that is the
    /// ingest's job and not the chunk stream's.
    pub timestamp: u32,

    /// What the message is.
    pub kind: MessageType,

    /// Which message stream it belongs to. Zero is the connection itself,
    /// where the protocol control messages live; a published stream gets its
    /// own from `createStream`.
    pub stream_id: u32,

    /// The message's bytes, with no chunk headers left in them.
    pub payload: Bytes,
}

/// A chunk stream id: which of the connection's parallel chunk streams a
/// chunk belongs to.
///
/// Checked on the way in because 0 and 1 are not identifiers at all — they
/// are the marks that say the real id follows in one or two more bytes — and
/// an id past 65 599 cannot be written down. Either would produce a stream
/// the peer reads as something else entirely.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChunkStreamId(u32);

impl ChunkStreamId {
    /// Protocol control messages. The only id the specification fixes.
    pub const CONTROL: Self = Self(2);
    /// AMF commands. Convention, and followed everywhere.
    pub const COMMAND: Self = Self(3);
    /// Audio. Convention.
    pub const AUDIO: Self = Self(4);
    /// Metadata. Convention.
    pub const DATA: Self = Self(5);
    /// Video. Convention, and apart from audio so that a large picture does
    /// not hold up sound.
    pub const VIDEO: Self = Self(6);

    /// Takes an id in 2..=65 599, or `None` for one that cannot be written.
    pub fn new(id: u32) -> Option<Self> {
        (2..=65_599).contains(&id).then_some(Self(id))
    }

    /// The id itself.
    pub fn value(self) -> u32 {
        self.0
    }
}

/// What can be wrong with a chunk stream.
///
/// Every one of these is a peer that is not speaking RTMP, and none can be
/// recovered from: the chunk stream's state is what says how to read the
/// next chunk, so once it is in doubt every byte after it is too. A
/// connection that sees one of these should close.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChunkError {
    /// A chunk left out fields with nothing on that chunk stream to take
    /// them from. The first chunk of a chunk stream has to be format 0.
    #[error(
        "chunk stream {csid} opened with format {fmt}, which inherits fields that do not exist"
    )]
    NothingToInherit { csid: u32, fmt: u8 },

    /// A chunk began a message on a chunk stream whose last one is still
    /// unfinished. A chunk stream carries one message at a time; the
    /// half-read one could only be dropped, and dropping it quietly sends a
    /// corrupt picture on.
    #[error("chunk stream {csid} began a message with {pending} bytes of the last one outstanding")]
    Interleaved { csid: u32, pending: usize },

    /// A message longer than this assembles. See [`MAX_ASSEMBLED_LENGTH`].
    #[error("message of {length} bytes is longer than the {limit} this assembles")]
    MessageTooLong { length: usize, limit: usize },

    /// More chunk streams than one connection has any use for. See
    /// [`MAX_CHUNK_STREAMS`].
    #[error("more than {MAX_CHUNK_STREAMS} chunk streams open")]
    TooManyChunkStreams,

    /// A chunk size of zero, or one past what the specification allows.
    #[error("chunk size {0} is not between 1 and {MAX_CHUNK_SIZE}")]
    InvalidChunkSize(u32),

    /// A protocol control message whose payload is not the four bytes it is
    /// defined as.
    #[error("{kind} message carries {length} bytes, not the 4 it is defined as")]
    MalformedControl { kind: &'static str, length: usize },
}

/// The fields a chunk stream carries forward, which is everything a chunk
/// with a shortened header leaves out.
#[derive(Clone, Copy, Debug)]
struct State {
    /// The current message's timestamp, absolute.
    timestamp: u32,
    /// The last delta seen, which a format 3 chunk starting a new message
    /// advances by.
    delta: u32,
    length: usize,
    kind: MessageType,
    stream_id: u32,
    /// Whether the current message's timestamp went in the four bytes after
    /// the header rather than in the header.
    extended: bool,
}

/// One chunk stream being reassembled: what its last chunk said, and the
/// bytes of the message it is part-way through.
struct ChunkStream {
    state: State,
    payload: BytesMut,
}

/// What one chunk turned out to be.
enum Step {
    /// It finished a message.
    Message(Box<Message>),
    /// It was taken in, and the message it belongs to is still incomplete.
    Absorbed,
    /// It is not all here yet, and nothing was consumed.
    NeedMore,
}

/// Reassembles a peer's chunks into messages.
///
/// One belongs to one connection: it holds the per-chunk-stream state that
/// says how to read a chunk with a shortened header, and that state means
/// nothing away from the peer it was built from.
pub struct Reader {
    streams: HashMap<u32, ChunkStream>,
    chunk_size: usize,
}

impl Default for Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl Reader {
    /// A reader at the chunk size a connection opens at.
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// The size of the peer's chunks, as it last said.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Takes the next whole message out of `buf`, or `None` if there is not
    /// one there yet.
    ///
    /// Consumes what it uses and leaves the rest, so a caller reads from its
    /// socket into the same buffer and calls again. A chunk that is only
    /// partly present consumes nothing, which is what makes calling this
    /// after every read safe.
    ///
    /// Call it until it returns `None` before reading more: one read can
    /// carry several messages, and a caller that took only the first would
    /// hold the rest until the next byte arrived — which, on a stream that
    /// has gone quiet, may be never.
    pub fn read(&mut self, buf: &mut BytesMut) -> Result<Option<Message>, ChunkError> {
        loop {
            match self.read_chunk(buf)? {
                Step::Message(message) => return Ok(Some(*message)),
                Step::Absorbed => continue,
                Step::NeedMore => return Ok(None),
            }
        }
    }

    fn read_chunk(&mut self, buf: &mut BytesMut) -> Result<Step, ChunkError> {
        let data = &buf[..];

        let Some((fmt, csid, mut pos)) = read_basic_header(data) else {
            return Ok(Step::NeedMore);
        };

        // Copied out rather than borrowed: the chunk stream is written to
        // further down, and nothing here is larger than a machine word.
        let previous = self
            .streams
            .get(&csid)
            .map(|stream| (stream.state, stream.payload.len()));

        match previous {
            None if fmt != 0 => return Err(ChunkError::NothingToInherit { csid, fmt }),
            None if self.streams.len() >= MAX_CHUNK_STREAMS => {
                return Err(ChunkError::TooManyChunkStreams);
            }
            Some((_, pending)) if fmt != 3 && pending > 0 => {
                return Err(ChunkError::Interleaved { csid, pending });
            }
            _ => {}
        }

        let state = match fmt {
            0 => {
                let Some(fields) = take(data, &mut pos, 11) else {
                    return Ok(Step::NeedMore);
                };
                let written = u24(&fields[0..3]);
                let length = u24(&fields[3..6]) as usize;
                let kind = MessageType::from_id(fields[6]);
                // The one little-endian field in the whole protocol.
                let stream_id = u32::from_le_bytes([fields[7], fields[8], fields[9], fields[10]]);
                let extended = written == EXTENDED_TIMESTAMP;
                let timestamp = if extended {
                    let Some(value) = take_u32(data, &mut pos) else {
                        return Ok(Step::NeedMore);
                    };
                    value
                } else {
                    written
                };
                State {
                    timestamp,
                    // Format 0 states a time rather than a step from one, so
                    // there is no delta yet for a format 3 chunk to repeat.
                    delta: 0,
                    length,
                    kind,
                    stream_id,
                    extended,
                }
            }
            1 | 2 => {
                let (previous, _) = previous.expect("a format above 0 has one");
                let Some(fields) = take(data, &mut pos, if fmt == 1 { 7 } else { 3 }) else {
                    return Ok(Step::NeedMore);
                };
                let written = u24(&fields[0..3]);
                let (length, kind) = if fmt == 1 {
                    (u24(&fields[3..6]) as usize, MessageType::from_id(fields[6]))
                } else {
                    (previous.length, previous.kind)
                };
                let extended = written == EXTENDED_TIMESTAMP;
                let delta = if extended {
                    let Some(value) = take_u32(data, &mut pos) else {
                        return Ok(Step::NeedMore);
                    };
                    value
                } else {
                    written
                };
                State {
                    // Wrapping because the protocol's counter does: a
                    // session running past 49 days is not an error.
                    timestamp: previous.timestamp.wrapping_add(delta),
                    delta,
                    length,
                    kind,
                    stream_id: previous.stream_id,
                    extended,
                }
            }
            _ => {
                let (previous, pending) = previous.expect("a format above 0 has one");
                // A message whose timestamp went in the four bytes after the
                // header has them repeated on every format 3 chunk that
                // follows. The specification says the opposite — that the
                // field is absent when the timestamp field is — but librtmp
                // writes them and so does everything descended from it,
                // which is every encoder.
                //
                // Read and discarded rather than used: what the four bytes
                // hold depends on the format being repeated, an absolute
                // time after format 0 and a delta after 1 or 2, and there is
                // nothing here that says which. The delta already on the
                // chunk stream says the same thing without the guess.
                if previous.extended && take_u32(data, &mut pos).is_none() {
                    return Ok(Step::NeedMore);
                }
                if pending > 0 {
                    // Mid-message: this is more of the same message, and
                    // nothing about it changes.
                    previous
                } else {
                    State {
                        timestamp: previous.timestamp.wrapping_add(previous.delta),
                        ..previous
                    }
                }
            }
        };

        if state.length > MAX_ASSEMBLED_LENGTH {
            return Err(ChunkError::MessageTooLong {
                length: state.length,
                limit: MAX_ASSEMBLED_LENGTH,
            });
        }

        let have = previous.map_or(0, |(_, pending)| pending);
        let wanted = (state.length - have).min(self.chunk_size);
        if buf.len() < pos + wanted {
            return Ok(Step::NeedMore);
        }

        // Every byte is here, so the borrow of `buf` can end and the chunk
        // come out of it.
        buf.advance(pos);
        let chunk = buf.split_to(wanted);

        let stream = self.streams.entry(csid).or_insert_with(|| ChunkStream {
            state,
            payload: BytesMut::new(),
        });
        stream.state = state;

        let complete = if stream.payload.is_empty() && chunk.len() == state.length {
            // The whole message came in one chunk, which is the ordinary
            // case for commands and for audio. Hand the buffer on rather
            // than copying it through the reassembly buffer.
            Some(chunk.freeze())
        } else {
            stream.payload.extend_from_slice(&chunk);
            (stream.payload.len() == state.length).then(|| stream.payload.split().freeze())
        };

        let Some(payload) = complete else {
            return Ok(Step::Absorbed);
        };
        let message = Message {
            timestamp: state.timestamp,
            kind: state.kind,
            stream_id: state.stream_id,
            payload,
        };
        self.apply(&message)?;
        Ok(Step::Message(Box::new(message)))
    }

    /// Acts on the messages that describe the chunk stream itself. See the
    /// module docs for why the reader does this rather than its caller.
    fn apply(&mut self, message: &Message) -> Result<(), ChunkError> {
        match message.kind {
            MessageType::SetChunkSize => {
                // The top bit is reserved and zero; masking it off rather
                // than refusing it keeps a peer that sets it working.
                let value = control_u32(message, "set chunk size")? & 0x7fff_ffff;
                if value == 0 || value as usize > MAX_CHUNK_SIZE {
                    return Err(ChunkError::InvalidChunkSize(value));
                }
                self.chunk_size = value as usize;
            }
            MessageType::Abort => {
                let csid = control_u32(message, "abort")?;
                if let Some(stream) = self.streams.get_mut(&csid) {
                    stream.payload.clear();
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Cuts messages into chunks.
///
/// One belongs to one connection, for the reason [`Reader`] does: the
/// shortened headers it writes are only readable against the state of the
/// chunk stream it wrote the last one on.
pub struct Writer {
    streams: HashMap<u32, State>,
    chunk_size: usize,
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

impl Writer {
    /// A writer at the chunk size a connection opens at.
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
            chunk_size: DEFAULT_CHUNK_SIZE,
        }
    }

    /// The size of the chunks this writes.
    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Changes it, for the messages written after this.
    ///
    /// The caller is the one that sends the `SetChunkSize` saying so, and
    /// has to send it before calling this: the peer goes on reading at the
    /// old size until that message reaches it.
    pub fn set_chunk_size(&mut self, size: usize) -> Result<(), ChunkError> {
        if size == 0 || size > MAX_CHUNK_SIZE {
            return Err(ChunkError::InvalidChunkSize(
                size.min(u32::MAX as usize) as u32
            ));
        }
        self.chunk_size = size;
        Ok(())
    }

    /// Writes `message` onto chunk stream `csid`, appending to `out`.
    ///
    /// Leaves out whatever the last message on that chunk stream already
    /// said. Which fields those are is decided here rather than by the
    /// caller, because the answer is a property of the chunk stream's state:
    /// a caller that got it wrong would produce a stream that parses into
    /// different messages than the ones it was given.
    pub fn write(
        &mut self,
        out: &mut BytesMut,
        csid: ChunkStreamId,
        message: &Message,
    ) -> Result<(), ChunkError> {
        let length = message.payload.len();
        if length > MAX_MESSAGE_LENGTH {
            return Err(ChunkError::MessageTooLong {
                length,
                limit: MAX_MESSAGE_LENGTH,
            });
        }
        let id = csid.value();

        // Format 0 restates everything, and is the only one that can express
        // a new message stream or a timestamp that went backwards — the
        // shorter formats can only add to the last one.
        let (fmt, written) = match self.streams.get(&id) {
            Some(previous)
                if previous.stream_id == message.stream_id
                    && message.timestamp >= previous.timestamp =>
            {
                let delta = message.timestamp - previous.timestamp;
                if previous.length != length || previous.kind != message.kind {
                    (1, delta)
                } else if previous.delta != delta {
                    (2, delta)
                } else {
                    (3, delta)
                }
            }
            _ => (0, message.timestamp),
        };

        let extended = written >= EXTENDED_TIMESTAMP;
        let field = if extended {
            EXTENDED_TIMESTAMP
        } else {
            written
        };

        put_basic_header(out, fmt, id);
        if fmt <= 2 {
            put_u24(out, field);
        }
        if fmt <= 1 {
            put_u24(out, length as u32);
            out.put_u8(message.kind.id());
        }
        if fmt == 0 {
            out.put_u32_le(message.stream_id);
        }
        if extended {
            out.put_u32(written);
        }

        let mut offset = 0;
        loop {
            let take = (length - offset).min(self.chunk_size);
            out.put_slice(&message.payload[offset..offset + take]);
            offset += take;
            if offset >= length {
                break;
            }
            // A continuation says nothing but which chunk stream it is on,
            // and repeats the extended timestamp if there was one — see the
            // note in `read_chunk` on why everything does.
            put_basic_header(out, 3, id);
            if extended {
                out.put_u32(written);
            }
        }

        self.streams.insert(
            id,
            State {
                timestamp: message.timestamp,
                delta: if fmt == 0 { 0 } else { written },
                length,
                kind: message.kind,
                stream_id: message.stream_id,
                extended,
            },
        );
        Ok(())
    }
}

/// Reads the format and chunk stream id, and says how many bytes they took.
///
/// The six bits that would hold the id are 0 or 1 when it is too large for
/// them, and the wider forms count from 64 because everything below that is
/// already expressible.
fn read_basic_header(data: &[u8]) -> Option<(u8, u32, usize)> {
    let first = *data.first()?;
    let fmt = first >> 6;
    match first & 0x3f {
        0 => Some((fmt, u32::from(*data.get(1)?) + 64, 2)),
        1 => Some((
            fmt,
            u32::from(u16::from_le_bytes([*data.get(1)?, *data.get(2)?])) + 64,
            3,
        )),
        id => Some((fmt, u32::from(id), 1)),
    }
}

fn put_basic_header(out: &mut BytesMut, fmt: u8, csid: u32) {
    let fmt = fmt << 6;
    match csid {
        0..=63 => out.put_u8(fmt | csid as u8),
        64..=319 => {
            out.put_u8(fmt);
            out.put_u8((csid - 64) as u8);
        }
        _ => {
            out.put_u8(fmt | 1);
            out.put_slice(&((csid - 64) as u16).to_le_bytes());
        }
    }
}

/// Takes `count` bytes from `pos`, advancing it, or `None` without touching
/// it when they are not all there.
fn take<'a>(data: &'a [u8], pos: &mut usize, count: usize) -> Option<&'a [u8]> {
    let slice = data.get(*pos..*pos + count)?;
    *pos += count;
    Some(slice)
}

fn take_u32(data: &[u8], pos: &mut usize) -> Option<u32> {
    let bytes = take(data, pos, 4)?;
    Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u24(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]])
}

fn put_u24(out: &mut BytesMut, value: u32) {
    out.put_slice(&value.to_be_bytes()[1..]);
}

/// The single big-endian `u32` that every protocol control message's payload
/// is.
fn control_u32(message: &Message, kind: &'static str) -> Result<u32, ChunkError> {
    let bytes: [u8; 4] =
        message.payload[..]
            .try_into()
            .map_err(|_| ChunkError::MalformedControl {
                kind,
                length: message.payload.len(),
            })?;
    Ok(u32::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(kind: MessageType, timestamp: u32, stream_id: u32, payload: &[u8]) -> Message {
        Message {
            timestamp,
            kind,
            stream_id,
            payload: Bytes::copy_from_slice(payload),
        }
    }

    fn video(timestamp: u32, payload: &[u8]) -> Message {
        message(MessageType::Video, timestamp, 1, payload)
    }

    /// Writes messages and reads them back at the same chunk size, which is
    /// the property that matters: what one side leaves out, the other has to
    /// put back.
    fn round_trip(messages: &[(ChunkStreamId, Message)], chunk_size: usize) -> Vec<Message> {
        let mut writer = Writer::new();
        writer.set_chunk_size(chunk_size).unwrap();
        let mut buf = BytesMut::new();
        for (csid, message) in messages {
            writer.write(&mut buf, *csid, message).unwrap();
        }

        let mut reader = Reader::new();
        reader.chunk_size = chunk_size;
        let mut read = Vec::new();
        while let Some(message) = reader.read(&mut buf).unwrap() {
            read.push(message);
        }
        assert!(buf.is_empty(), "{} bytes left over", buf.len());
        read
    }

    fn payloads(messages: &[(ChunkStreamId, Message)]) -> Vec<Message> {
        messages
            .iter()
            .map(|(_, message)| message.clone())
            .collect()
    }

    fn chunk0(
        csid: u8,
        timestamp: u32,
        length: usize,
        kind: u8,
        stream_id: u32,
        data: &[u8],
    ) -> BytesMut {
        let mut out = BytesMut::new();
        out.put_u8(csid);
        put_u24(&mut out, timestamp);
        put_u24(&mut out, length as u32);
        out.put_u8(kind);
        out.put_u32_le(stream_id);
        out.put_slice(data);
        out
    }

    fn chunk3(csid: u8, data: &[u8]) -> BytesMut {
        let mut out = BytesMut::new();
        out.put_u8(0xc0 | csid);
        out.put_slice(data);
        out
    }

    #[test]
    fn a_type_round_trips_through_its_id() {
        for id in 0..=u8::MAX {
            assert_eq!(MessageType::from_id(id).id(), id);
        }
    }

    #[test]
    fn a_chunk_stream_id_that_cannot_be_written_is_refused() {
        assert_eq!(ChunkStreamId::new(0), None);
        assert_eq!(ChunkStreamId::new(1), None);
        assert_eq!(ChunkStreamId::new(65_600), None);
        assert_eq!(ChunkStreamId::new(2).map(ChunkStreamId::value), Some(2));
    }

    #[test]
    fn a_message_that_fits_one_chunk_round_trips() {
        let sent = video(1000, b"one picture");
        assert_eq!(
            round_trip(&[(ChunkStreamId::VIDEO, sent.clone())], 4096),
            vec![sent]
        );
    }

    #[test]
    fn a_message_cut_into_chunks_is_put_back_together() {
        let sent = video(1000, &[0x17; 1000]);
        assert_eq!(
            round_trip(&[(ChunkStreamId::VIDEO, sent.clone())], DEFAULT_CHUNK_SIZE),
            vec![sent]
        );
    }

    #[test]
    fn every_basic_header_width_round_trips() {
        // 63 is the last id the six bits hold, 64 the first that needs a
        // second byte, and 320 the first that needs two.
        for id in [2, 63, 64, 319, 320, 65_599] {
            let csid = ChunkStreamId::new(id).expect("in range");
            let sent = video(0, b"payload");
            assert_eq!(
                round_trip(&[(csid, sent.clone())], 4096),
                vec![sent],
                "{id}"
            );
        }
    }

    #[test]
    fn a_run_of_messages_round_trips_through_the_shortened_headers() {
        // One length, type and stream throughout, at a fixed step: after the
        // first few the writer has nothing left to say and every header is
        // one byte.
        let sent: Vec<_> = (0..8u32)
            .map(|n| (ChunkStreamId::VIDEO, video(n * 33, &[n as u8; 40])))
            .collect();
        assert_eq!(round_trip(&sent, 4096), payloads(&sent));
    }

    #[test]
    fn each_header_format_is_used_as_soon_as_it_says_enough() {
        let sequence = [
            // The first message on a chunk stream has nothing behind it:
            // format 0, and a twelve-byte header.
            (video(0, b"aaaa"), 12),
            // A different message stream, which only format 0 can state.
            (message(MessageType::Video, 10, 2, b"aaaa"), 12),
            // A length that changed: format 1.
            (message(MessageType::Video, 20, 2, b"aaaaa"), 8),
            // Nothing new but the step from the last timestamp: format 2.
            (message(MessageType::Video, 45, 2, b"bbbbb"), 4),
            // The same step again, so there is nothing left to say at all
            // and a message costs one byte.
            (message(MessageType::Video, 70, 2, b"ccccc"), 1),
        ];

        let mut writer = Writer::new();
        let mut buf = BytesMut::new();
        for (message, header) in &sequence {
            let before = buf.len();
            writer
                .write(&mut buf, ChunkStreamId::VIDEO, message)
                .unwrap();
            assert_eq!(
                buf.len() - before,
                header + message.payload.len(),
                "{message:?}"
            );
        }

        // And what was left out comes back.
        let mut reader = Reader::new();
        for (message, _) in &sequence {
            assert_eq!(reader.read(&mut buf).unwrap().as_ref(), Some(message));
        }
        assert!(buf.is_empty());
    }

    #[test]
    fn a_stream_id_goes_out_little_endian() {
        let mut writer = Writer::new();
        let mut out = BytesMut::new();
        writer
            .write(
                &mut out,
                ChunkStreamId::VIDEO,
                &message(MessageType::Video, 0, 0x0102_0304, b""),
            )
            .unwrap();
        assert_eq!(&out[8..12], &[0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn an_incomplete_chunk_consumes_nothing() {
        let sent = video(1000, &[0x17; 300]);
        let mut writer = Writer::new();
        let mut whole = BytesMut::new();
        writer
            .write(&mut whole, ChunkStreamId::VIDEO, &sent)
            .unwrap();

        // Arriving in two pieces has to read the same as arriving in one, at
        // every point it could be cut — which holds only if a chunk that is
        // not all there leaves every one of its own bytes behind.
        for cut in 0..whole.len() {
            let mut reader = Reader::new();
            let mut buf = whole.clone().split_to(cut);
            assert_eq!(reader.read(&mut buf).unwrap(), None, "{cut}");
            buf.extend_from_slice(&whole[cut..]);
            assert_eq!(reader.read(&mut buf).unwrap(), Some(sent.clone()), "{cut}");
            assert!(buf.is_empty(), "{cut}");
        }
    }

    #[test]
    fn a_read_yields_every_message_already_in_the_buffer() {
        let sent = vec![
            (
                ChunkStreamId::COMMAND,
                message(MessageType::Amf0Command, 0, 0, b"connect"),
            ),
            (ChunkStreamId::VIDEO, video(0, b"picture")),
            (
                ChunkStreamId::AUDIO,
                message(MessageType::Audio, 0, 1, b"sound"),
            ),
        ];
        assert_eq!(round_trip(&sent, 4096), payloads(&sent));
    }

    #[test]
    fn messages_on_two_chunk_streams_interleave() {
        // Four bytes at a time, two eight-byte messages, alternating.
        let mut buf = BytesMut::new();
        buf.unsplit(chunk0(3, 10, 8, 20, 0, b"AAAA"));
        buf.unsplit(chunk0(4, 20, 8, 9, 1, b"BBBB"));
        buf.unsplit(chunk3(3, b"aaaa"));
        buf.unsplit(chunk3(4, b"bbbb"));

        let mut reader = Reader::new();
        reader.chunk_size = 4;
        let first = reader.read(&mut buf).unwrap().expect("A completes first");
        let second = reader.read(&mut buf).unwrap().expect("then B");
        assert_eq!(first.payload, Bytes::from_static(b"AAAAaaaa"));
        assert_eq!(first.kind, MessageType::Amf0Command);
        assert_eq!(second.payload, Bytes::from_static(b"BBBBbbbb"));
        assert_eq!(second.stream_id, 1);
        assert_eq!(reader.read(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_set_chunk_size_applies_to_the_chunks_after_it() {
        let mut buf = BytesMut::new();
        buf.unsplit(chunk0(2, 0, 4, 1, 0, &1024u32.to_be_bytes()));
        // 300 bytes in one chunk, which only parses at the new size.
        buf.unsplit(chunk0(6, 0, 300, 9, 1, &[0x17; 300]));

        let mut reader = Reader::new();
        let control = reader.read(&mut buf).unwrap().expect("the control message");
        assert_eq!(control.kind, MessageType::SetChunkSize);
        assert_eq!(reader.chunk_size(), 1024);
        let picture = reader.read(&mut buf).unwrap().expect("the picture");
        assert_eq!(picture.payload.len(), 300);
    }

    #[test]
    fn a_chunk_size_of_zero_is_refused() {
        let mut buf = chunk0(2, 0, 4, 1, 0, &0u32.to_be_bytes());
        assert_eq!(
            Reader::new().read(&mut buf),
            Err(ChunkError::InvalidChunkSize(0))
        );
    }

    #[test]
    fn an_abort_discards_the_half_read_message() {
        let mut buf = BytesMut::new();
        buf.unsplit(chunk0(6, 0, 8, 9, 1, b"AAAA"));
        buf.unsplit(chunk0(2, 0, 4, 2, 0, &6u32.to_be_bytes()));

        let mut reader = Reader::new();
        reader.chunk_size = 4;
        let abort = reader.read(&mut buf).unwrap().expect("the abort itself");
        assert_eq!(abort.kind, MessageType::Abort);

        // With the half-read message gone, chunk stream 6 may start another
        // — which is the whole point of an abort, and would be refused as an
        // interleave if the discard had not happened.
        buf.unsplit(chunk0(6, 0, 4, 9, 1, b"BBBB"));
        let next = reader.read(&mut buf).unwrap().expect("a new message");
        assert_eq!(next.payload, Bytes::from_static(b"BBBB"));
    }

    #[test]
    fn a_chunk_stream_cannot_open_with_an_inherited_header() {
        let mut buf = BytesMut::from(&[0xc6u8, 0, 0, 0][..]);
        assert_eq!(
            Reader::new().read(&mut buf),
            Err(ChunkError::NothingToInherit { csid: 6, fmt: 3 })
        );
    }

    #[test]
    fn a_message_begun_before_the_last_one_finished_is_refused() {
        let mut buf = BytesMut::new();
        buf.unsplit(chunk0(6, 0, 8, 9, 1, b"AAAA"));
        buf.unsplit(chunk0(6, 0, 4, 9, 1, b"BBBB"));

        let mut reader = Reader::new();
        reader.chunk_size = 4;
        assert_eq!(
            reader.read(&mut buf),
            Err(ChunkError::Interleaved {
                csid: 6,
                pending: 4
            })
        );
    }

    #[test]
    fn a_message_longer_than_this_assembles_is_refused() {
        let length = MAX_ASSEMBLED_LENGTH + 1;
        let mut buf = chunk0(6, 0, length, 9, 1, b"");
        assert_eq!(
            Reader::new().read(&mut buf),
            Err(ChunkError::MessageTooLong {
                length,
                limit: MAX_ASSEMBLED_LENGTH
            })
        );
    }

    #[test]
    fn more_chunk_streams_than_a_connection_uses_are_refused() {
        let mut writer = Writer::new();
        let mut buf = BytesMut::new();
        for id in 0..=MAX_CHUNK_STREAMS as u32 {
            let csid = ChunkStreamId::new(2 + id).expect("in range");
            writer.write(&mut buf, csid, &video(0, b"x")).unwrap();
        }
        let mut reader = Reader::new();
        let outcome = loop {
            match reader.read(&mut buf) {
                Ok(Some(_)) => continue,
                other => break other,
            }
        };
        assert_eq!(outcome, Err(ChunkError::TooManyChunkStreams));
    }

    #[test]
    fn a_timestamp_too_large_for_its_field_goes_after_the_header() {
        let sent = video(EXTENDED_TIMESTAMP + 5, &[0x17; 300]);
        let mut writer = Writer::new();
        let mut buf = BytesMut::new();
        writer.write(&mut buf, ChunkStreamId::VIDEO, &sent).unwrap();
        // The field says "look past the header", and the four bytes after it
        // hold the value.
        assert_eq!(&buf[1..4], &[0xff, 0xff, 0xff]);
        assert_eq!(&buf[12..16], &(EXTENDED_TIMESTAMP + 5).to_be_bytes());

        assert_eq!(Reader::new().read(&mut buf).unwrap(), Some(sent));
    }

    #[test]
    fn an_extended_timestamp_is_repeated_on_every_continuation() {
        // Two messages, so the second is written with a shortened header
        // against a chunk stream whose last message was extended, and both
        // span several chunks so the continuations carry the repeat.
        let sent = vec![
            (
                ChunkStreamId::VIDEO,
                video(EXTENDED_TIMESTAMP + 5, &[0x17; 400]),
            ),
            (
                ChunkStreamId::VIDEO,
                video(EXTENDED_TIMESTAMP + 38, &[0x27; 400]),
            ),
        ];
        assert_eq!(round_trip(&sent, DEFAULT_CHUNK_SIZE), payloads(&sent));
    }

    #[test]
    fn a_control_message_of_the_wrong_length_is_refused() {
        let mut buf = chunk0(2, 0, 2, 1, 0, &[0, 0]);
        assert_eq!(
            Reader::new().read(&mut buf),
            Err(ChunkError::MalformedControl {
                kind: "set chunk size",
                length: 2
            })
        );
    }

    #[test]
    fn an_empty_message_is_a_message() {
        let sent = message(MessageType::UserControl, 0, 0, b"");
        assert_eq!(
            round_trip(&[(ChunkStreamId::CONTROL, sent.clone())], 4096),
            vec![sent]
        );
    }
}
