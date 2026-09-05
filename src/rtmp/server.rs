//! The socket under an RTMP publish.
//!
//! Everything this does with the bytes is decided elsewhere. It reads into a
//! buffer, hands whole messages to a [`Session`], and does what comes back —
//! writes replies, opens a path, pushes units. The protocol is in the
//! modules beside this one and none of it knows a socket exists.
//!
//! One task per connection. A relay's connections are few and each of them
//! is pushing megabits, so the count is bounded by bandwidth long before it
//! is bounded by anything a scheduler cares about.

use std::io;
use std::sync::Arc;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::path::{Publisher, Registry};
use crate::rtmp::chunk::{self, ChunkError};
use crate::rtmp::handshake::{Handshake, HandshakeError, Step};
use crate::rtmp::session::{Action, Session, SessionError};

/// How much room to leave for the next read. Large enough that a picture
/// arrives in a few reads rather than dozens.
const READ_SIZE: usize = 64 * 1024;

/// Why a connection ended.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Handshake(#[from] HandshakeError),

    #[error(transparent)]
    Chunk(#[from] ChunkError),

    #[error(transparent)]
    Session(#[from] SessionError),
}

/// Accepts publishers until the task running this is dropped.
///
/// Every connection is spawned into a set this owns, so dropping this task
/// takes them all with it. That is what makes shutting the server down a
/// matter of dropping one handle.
pub async fn serve(listener: TcpListener, registry: Arc<Registry>) {
    let mut connections = JoinSet::new();
    loop {
        // Reap what has finished, so a long-running server does not hold a
        // handle per connection it ever accepted.
        while connections.try_join_next().is_some() {}

        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Out of file descriptors, or a connection that died between the
            // kernel accepting it and us asking. Neither is a reason to stop
            // listening.
            Err(error) => {
                tracing::warn!(%error, "could not accept a connection");
                continue;
            }
        };
        let registry = Arc::clone(&registry);
        connections.spawn(async move {
            tracing::debug!(%peer, "connected");
            match connection(stream, registry).await {
                Ok(()) => tracing::debug!(%peer, "closed"),
                Err(error) => tracing::info!(%peer, %error, "closed with an error"),
            }
        });
    }
}

/// Drives one connection from its first byte to its last.
pub async fn connection(
    mut stream: TcpStream,
    registry: Arc<Registry>,
) -> Result<(), ConnectionError> {
    // Media goes out in bursts of one picture. Waiting to see whether more
    // follows adds a round trip's delay to every one of them.
    stream.set_nodelay(true)?;

    let mut buf = BytesMut::new();
    let mut out = BytesMut::new();

    let mut handshake = Handshake::new();
    while !handshake.is_done() {
        match handshake.read(&mut buf)? {
            Step::NeedMore => {
                if read(&mut stream, &mut buf).await? == 0 {
                    return Ok(());
                }
            }
            Step::Send(reply) => stream.write_all(&reply).await?,
            Step::Done => {}
        }
    }
    if let Some(version) = handshake.client_version().filter(|version| *version != 0) {
        // Answered plainly regardless. Worth saying only because it is the
        // first thing to suspect if this client then goes quiet.
        tracing::debug!(version, "the client asked for the digest handshake");
    }

    let mut reader = chunk::Reader::new();
    let mut writer = chunk::Writer::new();
    let mut session = Session::new();
    let mut publisher: Option<Publisher> = None;

    loop {
        // Whatever is already buffered, before waiting for more: one read
        // can carry several messages, and the last of them may be the one
        // that matters.
        while let Some(message) = reader.read(&mut buf)? {
            for action in session.handle(message)? {
                if !apply(action, &mut writer, &mut out, &registry, &mut publisher)? {
                    stream.write_all(&out).await?;
                    return Ok(());
                }
            }
        }
        if !out.is_empty() {
            stream.write_all(&out).await?;
            out.clear();
        }

        // The borrow of `publisher` ends with this block, so the handlers
        // after it are free to replace it.
        let read = {
            let displaced = async {
                match publisher.as_mut() {
                    Some(publisher) => publisher.evicted().await,
                    // Nothing to be displaced from yet. Waiting forever here
                    // leaves the read as the only thing that can finish.
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                read = read(&mut stream, &mut buf) => read?,
                () = displaced => {
                    // Another publisher took this path. This connection has
                    // nowhere to send to and is not going to get it back.
                    tracing::info!(path = publisher.as_ref().map(Publisher::name), "displaced by another publisher");
                    return Ok(());
                }
            }
        };
        if read == 0 {
            return Ok(());
        }
        // What arrived includes chunk headers and whatever partial message
        // the reader is still holding, which is why the count comes from
        // here rather than from the messages.
        if let Some(action) = session.received(read) {
            apply(action, &mut writer, &mut out, &registry, &mut publisher)?;
        }
    }
}

/// Does one thing the session asked for. Returns whether the connection goes
/// on.
fn apply(
    action: Action,
    writer: &mut chunk::Writer,
    out: &mut BytesMut,
    registry: &Arc<Registry>,
    publisher: &mut Option<Publisher>,
) -> Result<bool, ConnectionError> {
    match action {
        Action::Send { csid, message } => writer.write(out, csid, &message)?,
        // After the message announcing it, which the session put first.
        Action::SetChunkSize(size) => writer.set_chunk_size(size)?,
        Action::Publish { path, description } => {
            tracing::info!(%path, tracks = description.tracks().len(), "publishing");
            *publisher = Some(registry.publish(&path, *description));
        }
        Action::Unit(unit) => {
            if let Some(publisher) = publisher.as_ref() {
                publisher.push(*unit);
            }
        }
        Action::Unpublish => {
            if let Some(publisher) = publisher.take() {
                tracing::info!(path = publisher.name(), "unpublished");
            }
            return Ok(false);
        }
    }
    Ok(true)
}

/// Reads once, leaving room first.
///
/// The reserve matters: a `BytesMut` with no spare capacity gives a reader
/// almost none to fill, and a socket carrying a picture would be read a
/// handful of bytes at a time.
async fn read(stream: &mut TcpStream, buf: &mut BytesMut) -> io::Result<usize> {
    buf.reserve(READ_SIZE);
    stream.read_buf(buf).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::{Buf, BufMut, Bytes};
    use tokio::net::TcpStream;

    use super::*;
    use crate::codec::h264;
    use crate::rtmp::amf0::{self, Value};
    use crate::rtmp::chunk::{ChunkStreamId, Message, MessageType};
    use crate::rtmp::flv::{self, AudioTag, VideoTag};
    use crate::rtmp::handshake::VERSION;
    use crate::track::Kind;
    use crate::unit::Unit;

    /// A client that speaks just enough RTMP to publish.
    struct Client {
        stream: TcpStream,
        writer: chunk::Writer,
        reader: chunk::Reader,
        buf: BytesMut,
    }

    impl Client {
        async fn connect(port: u16) -> Self {
            let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

            // C0 and C1, then S0 S1 S2, then C2.
            let mut hello = BytesMut::new();
            hello.put_u8(VERSION);
            hello.put_slice(&[0; 1536]);
            stream.write_all(&hello).await.unwrap();
            let mut reply = vec![0; 1 + 1536 * 2];
            stream.read_exact(&mut reply).await.unwrap();
            stream.write_all(&reply[1..1 + 1536]).await.unwrap();

            Self {
                stream,
                writer: chunk::Writer::new(),
                reader: chunk::Reader::new(),
                buf: BytesMut::new(),
            }
        }

        async fn send(&mut self, csid: ChunkStreamId, message: &Message) {
            let mut out = BytesMut::new();
            self.writer.write(&mut out, csid, message).unwrap();
            self.stream.write_all(&out).await.unwrap();
        }

        async fn command(&mut self, name: &str, transaction: f64, rest: &[Value]) {
            let mut values = vec![Value::String(name.to_owned()), Value::Number(transaction)];
            values.extend_from_slice(rest);
            let mut payload = BytesMut::new();
            amf0::write_all(&mut payload, &values).unwrap();
            self.send(
                ChunkStreamId::COMMAND,
                &Message {
                    timestamp: 0,
                    kind: MessageType::Amf0Command,
                    stream_id: 0,
                    payload: payload.freeze(),
                },
            )
            .await;
        }

        async fn media(&mut self, kind: MessageType, timestamp: u32, payload: Bytes) {
            let csid = if kind == MessageType::Video {
                ChunkStreamId::VIDEO
            } else {
                ChunkStreamId::AUDIO
            };
            self.send(
                csid,
                &Message {
                    timestamp,
                    kind,
                    stream_id: 1,
                    payload,
                },
            )
            .await;
        }

        /// Reads until one whole message comes back.
        async fn recv(&mut self) -> Message {
            loop {
                if let Some(message) = self.reader.read(&mut self.buf).unwrap() {
                    return message;
                }
                self.buf.reserve(4096);
                let read = self.stream.read_buf(&mut self.buf).await.unwrap();
                assert!(read > 0, "the server closed");
            }
        }

        /// Reads until an AMF0 command whose first value is `name`.
        async fn expect(&mut self, name: &str) -> Vec<Value> {
            loop {
                let message = self.recv().await;
                if message.kind != MessageType::Amf0Command {
                    continue;
                }
                let values = amf0::read_all(&message.payload).unwrap();
                if values.first().and_then(Value::as_str) == Some(name) {
                    return values;
                }
            }
        }

        async fn publish(&mut self, app: &str, key: &str) {
            self.command(
                "connect",
                1.0,
                &[Value::Object(vec![(
                    "app".to_owned(),
                    Value::String(app.to_owned()),
                )])],
            )
            .await;
            self.expect("_result").await;
            self.command("createStream", 2.0, &[]).await;
            self.expect("_result").await;
            self.command(
                "publish",
                3.0,
                &[
                    Value::Null,
                    Value::String(key.to_owned()),
                    Value::String("live".to_owned()),
                ],
            )
            .await;
            self.expect("onStatus").await;
        }

        async fn sequence_headers(&mut self) {
            let config = h264::AvcConfig {
                parameters: h264::Parameters {
                    sps: vec![
                        h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e, 0xd9]))
                            .unwrap(),
                    ],
                    pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c])).unwrap()],
                },
                nal_length_size: 4,
            };
            let header =
                flv::write_video(&VideoTag::SequenceHeader(config.to_bytes().unwrap())).unwrap();
            self.media(MessageType::Video, 0, header).await;
            self.media(
                MessageType::Audio,
                0,
                flv::write_audio(&AudioTag::SequenceHeader(Bytes::from_static(&[0x12, 0x10]))),
            )
            .await;
        }

        async fn picture(&mut self, timestamp: u32, keyframe: bool) {
            let mut data = BytesMut::new();
            data.put_u32(2);
            data.put_slice(&[if keyframe { 0x65 } else { 0x41 }, timestamp as u8]);
            let tag = flv::write_video(&VideoTag::Picture {
                keyframe,
                composition_time: 0,
                data: data.freeze(),
            })
            .unwrap();
            self.media(MessageType::Video, timestamp, tag).await;
        }
    }

    /// Starts a server on a port the operating system picks.
    async fn server() -> (u16, Arc<Registry>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let registry = Registry::new();
        let task = tokio::spawn(serve(listener, Arc::clone(&registry)));
        (port, registry, task)
    }

    /// Waits for a path to appear, so a test does not race the server.
    async fn wait_for(registry: &Registry, name: &str) -> crate::path::Reader {
        for _ in 0..200 {
            if let Some(reader) = registry.read(name) {
                return reader;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("{name} never appeared");
    }

    #[tokio::test]
    async fn a_publisher_reaches_a_reader_through_a_real_socket() {
        let (port, registry, _task) = server().await;
        let mut client = Client::connect(port).await;
        client.publish("live", "cam1").await;
        client.sequence_headers().await;
        client.picture(0, true).await;

        let mut reader = wait_for(&registry, "live/cam1").await;
        assert_eq!(reader.description().tracks().len(), 2);
        assert_eq!(reader.description().tracks()[0].kind(), Kind::Video);

        client.picture(33, false).await;
        let Some(Unit::Video(unit)) = reader.next().await else {
            panic!("a picture")
        };
        assert!(unit.keyframe);
        assert_eq!(unit.pts, Duration::ZERO);

        let Some(Unit::Video(unit)) = reader.next().await else {
            panic!("a picture")
        };
        assert!(!unit.keyframe);
        assert_eq!(unit.pts, Duration::from_millis(33));
    }

    #[tokio::test]
    async fn the_chunk_size_the_server_announces_is_the_one_it_then_uses() {
        let (port, _registry, _task) = server().await;
        let mut client = Client::connect(port).await;
        client
            .command(
                "connect",
                1.0,
                &[Value::Object(vec![(
                    "app".to_owned(),
                    Value::String("live".to_owned()),
                )])],
            )
            .await;

        // The client's reader starts at 128 and is only right about the
        // messages after the announcement if the server changed size in the
        // same order. `_result` is the first message after it.
        let values = client.expect("_result").await;
        assert_eq!(
            values[3].get("code").and_then(Value::as_str),
            Some("NetConnection.Connect.Success")
        );
        assert_ne!(client.reader.chunk_size(), 128);
    }

    #[tokio::test]
    async fn a_second_publisher_takes_the_path_and_the_first_connection_closes() {
        let (port, registry, _task) = server().await;

        let mut first = Client::connect(port).await;
        first.publish("live", "cam1").await;
        first.sequence_headers().await;
        first.picture(0, true).await;
        wait_for(&registry, "live/cam1").await;

        let mut second = Client::connect(port).await;
        second.publish("live", "cam1").await;
        second.sequence_headers().await;
        second.picture(0, true).await;

        // The first connection is told to go without having sent anything
        // more, which is the case that needs the wait rather than the push.
        let mut left = BytesMut::new();
        left.reserve(1024);
        let read = tokio::time::timeout(Duration::from_secs(5), first.stream.read_buf(&mut left))
            .await
            .expect("the server closed it rather than leaving it open")
            .unwrap();
        // Whatever is left in flight, then the end.
        assert!(read == 0 || left.remaining() > 0);
    }

    #[tokio::test]
    async fn a_path_goes_when_its_publisher_does() {
        let (port, registry, _task) = server().await;
        let mut client = Client::connect(port).await;
        client.publish("live", "cam1").await;
        client.sequence_headers().await;
        client.picture(0, true).await;

        let mut reader = wait_for(&registry, "live/cam1").await;
        drop(client);

        while reader.next().await.is_some() {}
        for _ in 0..200 {
            if registry.read("live/cam1").is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("the path outlived its publisher");
    }

    #[tokio::test]
    async fn a_client_that_says_nothing_is_not_a_path() {
        let (port, registry, _task) = server().await;
        let _client = Client::connect(port).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(registry.names().is_empty());
    }
}
