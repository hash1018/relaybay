//! The socket under an RTSP play.
//!
//! Reads requests, hands them to a [`Session`], and does what comes back.
//! Once a client says `PLAY` the same loop is doing two things at once —
//! waiting for whatever the client says next, and waiting for the next unit
//! of the stream — which is the shape the whole protocol is built around.
//!
//! # Where the packets go
//!
//! Back down this connection, wrapped in the four bytes [`message::interleave`]
//! adds. A client that will only take them over UDP is turned down at
//! `SETUP` and asks again for this — see [`Session::accepting_udp`].
//!
//! One socket for everything is what makes an RTSP stream work through a
//! firewall that would drop the UDP, and it costs a copy through the kernel
//! that UDP would not. For a relay whose readers are counted in tens, that
//! is the better trade; for one serving a stadium it would not be.
//!
//! # A client that stops reading
//!
//! Every write has a deadline. A reader that has stopped taking bytes fills
//! its kernel buffer and then this one's, and without a deadline the task
//! would wait on it for as long as the connection stayed open — holding a
//! reader, a set of packetizers and whatever the socket had buffered. The
//! path layer already drops what such a reader misses; this is what stops
//! the connection itself from lingering.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::path::{Reader, Registry};
use crate::rtp::{self, Stream};
use crate::rtsp::message::{self, Incoming, RtspError, Transport};
use crate::rtsp::session::{Action, Session, Setup};
use crate::track::{Codec, TrackId};
use crate::unit::{AudioPayload, Unit, VideoPayload};

/// How much room to leave for the next read.
const READ_SIZE: usize = 16 * 1024;

/// How long a write may take before the client is taken to have stopped
/// reading. See the module docs.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Tells one connection's streams apart from another's.
static SSRCS: AtomicU32 = AtomicU32::new(1);

/// Why a connection ended.
#[derive(Debug, thiserror::Error)]
pub enum ConnectionError {
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Rtsp(#[from] RtspError),

    #[error("the client stopped reading")]
    Stalled,
}

/// Accepts players until the task running this is dropped.
pub async fn serve(listener: TcpListener, registry: Arc<Registry>) {
    let mut connections = JoinSet::new();
    loop {
        while connections.try_join_next().is_some() {}

        let (stream, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
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

/// Drives one connection from its first request to its last packet.
pub async fn connection(
    mut stream: TcpStream,
    registry: Arc<Registry>,
) -> Result<(), ConnectionError> {
    stream.set_nodelay(true)?;
    let mut buf = BytesMut::new();
    let mut out = BytesMut::new();
    // UDP is not sent here yet, so it is not offered: see the module docs.
    let mut session = Session::new().accepting_udp(false);
    let mut playing: Option<Playing> = None;

    loop {
        while let Some(incoming) = message::read(&mut buf)? {
            let request = match incoming {
                // RTCP from the client — a receiver report, saying what it
                // has and has not had. Nothing acts on one yet; reading past
                // it is what keeps the connection in step.
                Incoming::Interleaved { .. } => continue,
                Incoming::Request(request) => request,
            };
            tracing::debug!(method = request.method.name(), uri = request.uri, "request");

            for action in session.handle(&request, |path| registry.describe(path)) {
                match action {
                    Action::Respond(response) => out.put_slice(&response.to_bytes()),
                    Action::Play { path, tracks } => {
                        playing = attach(&registry, &path, &tracks);
                        if playing.is_none() {
                            // The publisher went between DESCRIBE and PLAY.
                            // The client is told by the stream ending, which
                            // is what closing does.
                            tracing::info!(%path, "gone before it could be played");
                            write(&mut stream, &mut out).await?;
                            return Ok(());
                        }
                        tracing::info!(%path, tracks = tracks.len(), "playing");
                    }
                    Action::Pause => playing = None,
                    Action::Teardown => {
                        write(&mut stream, &mut out).await?;
                        return Ok(());
                    }
                }
            }
        }
        write(&mut stream, &mut out).await?;

        tokio::select! {
            read = read(&mut stream, &mut buf) => {
                if read? == 0 {
                    return Ok(());
                }
            }
            unit = next(playing.as_mut()) => {
                let Some(unit) = unit else {
                    // The publisher has gone. There is nothing more to send
                    // and no way to say so in RTSP but to close.
                    tracing::info!(path = session.path(), "the stream ended");
                    return Ok(());
                };
                if let Some(playing) = playing.as_mut() {
                    playing.write(&mut out, &unit);
                }
                write(&mut stream, &mut out).await?;
            }
        }
    }
}

/// What is being sent, once a client has said to start.
struct Playing {
    reader: Reader,
    tracks: Vec<Bound>,
}

/// One track the client asked for, and what turns its units into packets.
struct Bound {
    track: TrackId,
    channel: u8,
    packetizer: Packetizer,
}

enum Packetizer {
    H264(rtp::h264::Packetizer),
    Aac(rtp::aac::Packetizer),
}

impl Playing {
    /// Turns one unit into packets and wraps each for the connection.
    ///
    /// A unit for a track the client did not set up goes nowhere, which is
    /// what a client watching only the video of a stream with sound asked
    /// for.
    fn write(&mut self, out: &mut BytesMut, unit: &Unit) {
        let Some(bound) = self
            .tracks
            .iter_mut()
            .find(|bound| bound.track == unit.track())
        else {
            return;
        };
        let packets = match (&mut bound.packetizer, unit) {
            (Packetizer::H264(packetizer), Unit::Video(unit)) => {
                let VideoPayload::H264(nalus) = &unit.payload;
                packetizer.packetize(nalus, unit.pts)
            }
            (Packetizer::Aac(packetizer), Unit::Audio(unit)) => {
                let AudioPayload::Aac(frame) = &unit.payload;
                match packetizer.packetize(frame, unit.pts) {
                    Ok(packets) => packets,
                    // One frame this cannot express. Dropping it costs the
                    // listener a click; ending the connection costs them the
                    // stream.
                    Err(error) => {
                        tracing::warn!(%error, "a frame that could not be packetized");
                        return;
                    }
                }
            }
            // A track's codec does not change under it — a description is
            // fixed for the life of a stream — so this is unreachable rather
            // than merely unlikely.
            _ => return,
        };
        for packet in packets {
            if let Err(error) = message::interleave(out, bound.channel, &packet) {
                tracing::warn!(%error, "a packet too large to interleave");
            }
        }
    }
}

/// Attaches to a path and prepares a packetizer per track the client set up.
fn attach(registry: &Registry, path: &str, setups: &[Setup]) -> Option<Playing> {
    let reader = registry.read(path)?;
    let description = reader.description().clone();
    let tracks = setups
        .iter()
        .filter_map(|setup| {
            let track = description.track(setup.track)?;
            let Transport::Interleaved { rtp: channel, .. } = setup.transport else {
                // Refused at SETUP, so a client cannot reach this by asking.
                return None;
            };
            let ssrc = SSRCS.fetch_add(1, Ordering::Relaxed);
            let packetizer = match track.codec() {
                Codec::H264(_) => Packetizer::H264(rtp::h264::Packetizer::new(Stream::new(
                    ssrc,
                    rtp::payload_type::VIDEO,
                    track.codec().clock_rate(),
                ))),
                Codec::Aac(_) => Packetizer::Aac(rtp::aac::Packetizer::new(Stream::new(
                    ssrc,
                    rtp::payload_type::AUDIO,
                    track.codec().clock_rate(),
                ))),
            };
            Some(Bound {
                track: setup.track,
                channel,
                packetizer,
            })
        })
        .collect();
    Some(Playing { reader, tracks })
}

/// The next unit, or a wait that never ends when nothing is playing.
///
/// Cancel-safe on both sides, so it can sit in a `select!` beside a read.
async fn next(playing: Option<&mut Playing>) -> Option<Unit> {
    match playing {
        Some(playing) => playing.reader.next().await,
        None => std::future::pending().await,
    }
}

async fn read(stream: &mut TcpStream, buf: &mut BytesMut) -> io::Result<usize> {
    buf.reserve(READ_SIZE);
    stream.read_buf(buf).await
}

/// Writes what has been built up, and gives up on a client that has stopped
/// taking it. See the module docs.
async fn write(stream: &mut TcpStream, out: &mut BytesMut) -> Result<(), ConnectionError> {
    if out.is_empty() {
        return Ok(());
    }
    let written = tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(out))
        .await
        .map_err(|_| ConnectionError::Stalled)?;
    out.clear();
    written.map_err(ConnectionError::Io)
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::codec::{aac, h264};
    use crate::rtp::Header;
    use crate::rtsp::message::{Method, Request, Response, Status};
    use crate::track::{Codec, Description};
    use crate::unit::{AudioUnit, VideoUnit};

    fn description() -> Description {
        Description::new(vec![
            Codec::H264(h264::Parameters {
                sps: vec![
                    h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e, 0xd9])).unwrap(),
                ],
                pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce, 0x3c])).unwrap()],
            }),
            Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()),
        ])
        .unwrap()
    }

    fn picture(pts: u64, keyframe: bool, length: usize) -> Unit {
        let mut data = vec![if keyframe { 0x65 } else { 0x41 }];
        data.extend(std::iter::repeat_n(0xab, length));
        Unit::Video(VideoUnit::new(
            description().tracks()[0].id(),
            VideoPayload::H264(vec![h264::Nal::new(Bytes::from_owner(data)).unwrap()]),
            Duration::from_millis(pts),
            Duration::from_millis(pts),
        ))
    }

    fn sound(pts: u64) -> Unit {
        Unit::Audio(AudioUnit {
            track: description().tracks()[1].id(),
            payload: AudioPayload::Aac(Bytes::from_static(&[0x21, 0x00, 0x03])),
            pts: Duration::from_millis(pts),
        })
    }

    /// A client that speaks just enough RTSP to play.
    struct Client {
        stream: TcpStream,
        buf: BytesMut,
        sequence: u32,
    }

    impl Client {
        async fn connect(port: u16) -> Self {
            Self {
                stream: TcpStream::connect(("127.0.0.1", port)).await.unwrap(),
                buf: BytesMut::new(),
                sequence: 0,
            }
        }

        async fn send(&mut self, method: &str, uri: &str, headers: &[(&str, &str)]) -> Response {
            self.sequence += 1;
            let mut request = format!("{method} {uri} RTSP/1.0\r\nCSeq: {}\r\n", self.sequence);
            for (name, value) in headers {
                request.push_str(&format!("{name}: {value}\r\n"));
            }
            request.push_str("\r\n");
            self.stream.write_all(request.as_bytes()).await.unwrap();
            self.response().await
        }

        /// Reads until a response, skipping any media that arrives first.
        async fn response(&mut self) -> Response {
            loop {
                if let Some(response) = self.take_response() {
                    return response;
                }
                self.buf.reserve(8192);
                let read = self.stream.read_buf(&mut self.buf).await.unwrap();
                assert!(read > 0, "the server closed");
            }
        }

        /// Reads the head of a response out of the buffer, dropping any
        /// interleaved frames in front of it.
        fn take_response(&mut self) -> Option<Response> {
            loop {
                match self.buf.first()? {
                    b'$' => {
                        let head = self.buf.get(..4)?;
                        let length = usize::from(u16::from_be_bytes([head[2], head[3]]));
                        if self.buf.len() < 4 + length {
                            return None;
                        }
                        let _ = self.buf.split_to(4 + length);
                    }
                    _ => return self.parse_response(),
                }
            }
        }

        fn parse_response(&mut self) -> Option<Response> {
            let end = self
                .buf
                .windows(4)
                .position(|window| window == b"\r\n\r\n")?;
            let head = String::from_utf8(self.buf[..end].to_vec()).unwrap();
            let mut lines = head.split("\r\n");
            let status = lines.next().unwrap().split_whitespace().nth(1).unwrap();
            let mut response = Response::new(Status(status.parse().unwrap(), ""));
            let mut length = 0;
            for line in lines {
                let (name, value) = line.split_once(':').unwrap();
                if name.eq_ignore_ascii_case("Content-Length") {
                    length = value.trim().parse().unwrap();
                }
                response.headers.push(name.trim(), value.trim());
            }
            if self.buf.len() < end + 4 + length {
                return None;
            }
            let _ = self.buf.split_to(end + 4);
            response.body = self.buf.split_to(length).freeze();
            Some(response)
        }

        /// Reads the next interleaved frame, skipping any response first.
        async fn frame(&mut self) -> (u8, Bytes) {
            loop {
                if self.buf.first() == Some(&b'$') {
                    if let Some(head) = self.buf.get(..4) {
                        let channel = head[1];
                        let length = usize::from(u16::from_be_bytes([head[2], head[3]]));
                        if self.buf.len() >= 4 + length {
                            let _ = self.buf.split_to(4);
                            return (channel, self.buf.split_to(length).freeze());
                        }
                    }
                } else if !self.buf.is_empty() && self.take_response().is_some() {
                    continue;
                }
                self.buf.reserve(8192);
                let read = self.stream.read_buf(&mut self.buf).await.unwrap();
                assert!(read > 0, "the server closed");
            }
        }
    }

    async fn server() -> (u16, Arc<Registry>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let registry = Registry::new();
        let task = tokio::spawn(serve(listener, Arc::clone(&registry)));
        (port, registry, task)
    }

    #[tokio::test]
    async fn a_client_describes_sets_up_and_plays() {
        let (port, registry, _task) = server().await;
        let publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;

        let options = client.send("OPTIONS", "*", &[]).await;
        assert_eq!(options.status.0, 200);
        assert!(options.headers.get("Public").unwrap().contains("PLAY"));

        let describe = client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        assert_eq!(describe.status.0, 200);
        let sdp = String::from_utf8(describe.body.to_vec()).unwrap();
        assert!(sdp.contains("m=video"), "{sdp}");
        assert!(sdp.contains("sprop-parameter-sets="), "{sdp}");

        let setup = client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        assert_eq!(setup.status.0, 200);
        assert!(setup.headers.get("Session").is_some());

        let play = client.send("PLAY", "rtsp://127.0.0.1/live/cam1", &[]).await;
        assert_eq!(play.status.0, 200);

        publisher.push(picture(0, true, 100));
        let (channel, packet) = client.frame().await;
        assert_eq!(channel, 0);
        let (header, payload) = Header::parse(&packet).unwrap();
        assert_eq!(header.payload_type, rtp::payload_type::VIDEO);
        assert!(
            header.marker,
            "one small picture is one packet, and its end"
        );
        assert_eq!(payload[0], 0x65, "the NAL unit, unchanged");
    }

    #[tokio::test]
    async fn a_picture_too_large_for_a_packet_arrives_in_pieces() {
        let (port, registry, _task) = server().await;
        let publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        client.send("PLAY", "rtsp://127.0.0.1/live/cam1", &[]).await;

        publisher.push(picture(0, true, rtp::MTU * 3));
        let mut rebuilt = Vec::new();
        loop {
            let (_, packet) = client.frame().await;
            let (header, payload) = Header::parse(&packet).unwrap();
            assert_eq!(payload[0] & 0x1f, 28, "a fragment");
            if payload[1] & 0x80 != 0 {
                rebuilt.push((payload[0] & 0xe0) | (payload[1] & 0x1f));
            }
            rebuilt.extend_from_slice(&payload[2..]);
            if header.marker {
                break;
            }
        }
        assert_eq!(rebuilt.len(), rtp::MTU * 3 + 1);
        assert_eq!(rebuilt[0], 0x65);
    }

    #[tokio::test]
    async fn two_tracks_go_out_on_the_channels_they_were_set_up_on() {
        let (port, registry, _task) = server().await;
        let publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        for (track, channels) in [(0, "0-1"), (1, "2-3")] {
            client
                .send(
                    "SETUP",
                    &format!("rtsp://127.0.0.1/live/cam1/trackID={track}"),
                    &[(
                        "Transport",
                        &format!("RTP/AVP/TCP;unicast;interleaved={channels}"),
                    )],
                )
                .await;
        }
        client.send("PLAY", "rtsp://127.0.0.1/live/cam1", &[]).await;

        publisher.push(picture(0, true, 50));
        publisher.push(sound(20));

        let (video, packet) = client.frame().await;
        assert_eq!(video, 0);
        assert_eq!(
            Header::parse(&packet).unwrap().0.payload_type,
            rtp::payload_type::VIDEO
        );

        let (audio, packet) = client.frame().await;
        assert_eq!(audio, 2);
        let (header, payload) = Header::parse(&packet).unwrap();
        assert_eq!(header.payload_type, rtp::payload_type::AUDIO);
        // One AU header of sixteen bits, then the frame.
        assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 16);
        assert_eq!(&payload[4..], &[0x21, 0x00, 0x03]);
    }

    #[tokio::test]
    async fn a_track_the_client_did_not_set_up_is_not_sent() {
        let (port, registry, _task) = server().await;
        let publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=1",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=2-3")],
            )
            .await;
        client.send("PLAY", "rtsp://127.0.0.1/live/cam1", &[]).await;

        publisher.push(picture(0, true, 50));
        publisher.push(sound(20));

        // The picture went nowhere; the sound is the first thing that comes.
        let (channel, packet) = client.frame().await;
        assert_eq!(channel, 2);
        assert_eq!(
            Header::parse(&packet).unwrap().0.payload_type,
            rtp::payload_type::AUDIO
        );
    }

    #[tokio::test]
    async fn a_client_that_will_only_take_udp_is_told_to_ask_again() {
        let (port, registry, _task) = server().await;
        let _publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;

        let refused = client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP;unicast;client_port=8000-8001")],
            )
            .await;
        assert_eq!(refused.status.0, 461);

        // And the connection goes on, which is the point of answering rather
        // than closing.
        let accepted = client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        assert_eq!(accepted.status.0, 200);
    }

    #[tokio::test]
    async fn describing_a_path_nothing_is_publishing_to_is_answered_not_found() {
        let (port, _registry, _task) = server().await;
        let mut client = Client::connect(port).await;
        let response = client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/nobody", &[])
            .await;
        assert_eq!(response.status.0, 404);
    }

    #[tokio::test]
    async fn a_client_that_tears_down_is_answered_and_then_closed() {
        let (port, registry, _task) = server().await;
        let _publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        let response = client
            .send("TEARDOWN", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        assert_eq!(response.status.0, 200);

        let mut left = BytesMut::new();
        left.reserve(64);
        assert_eq!(client.stream.read_buf(&mut left).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_client_is_closed_when_the_publisher_goes() {
        let (port, registry, _task) = server().await;
        let publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send("DESCRIBE", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;
        client.send("PLAY", "rtsp://127.0.0.1/live/cam1", &[]).await;
        publisher.push(picture(0, true, 50));
        client.frame().await;

        drop(publisher);
        // RTSP has no way to say the stream ended but to stop being there.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let mut left = BytesMut::new();
            left.reserve(4096);
            let read = tokio::time::timeout_at(deadline, client.stream.read_buf(&mut left))
                .await
                .expect("the server closed the connection")
                .unwrap();
            if read == 0 {
                return;
            }
        }
    }

    #[tokio::test]
    async fn an_interleaved_report_from_the_client_does_not_confuse_the_connection() {
        let (port, registry, _task) = server().await;
        let _publisher = registry.publish("live/cam1", description());
        let mut client = Client::connect(port).await;
        client
            .send(
                "SETUP",
                "rtsp://127.0.0.1/live/cam1/trackID=0",
                &[("Transport", "RTP/AVP/TCP;unicast;interleaved=0-1")],
            )
            .await;

        // A receiver report on the second channel, then a request. If the
        // frame were not read past, the request after it would be read as
        // rubbish.
        let mut report = BytesMut::new();
        message::interleave(&mut report, 1, &[0x81, 0xc9, 0x00, 0x07]).unwrap();
        client.stream.write_all(&report).await.unwrap();

        let response = client
            .send("GET_PARAMETER", "rtsp://127.0.0.1/live/cam1", &[])
            .await;
        assert_eq!(response.status.0, 200);
    }

    #[tokio::test]
    async fn a_request_this_does_not_serve_is_refused_without_closing() {
        let (port, _registry, _task) = server().await;
        let mut client = Client::connect(port).await;
        assert_eq!(
            client
                .send("RECORD", "rtsp://127.0.0.1/live/cam1", &[])
                .await
                .status
                .0,
            405
        );
        assert_eq!(client.send("OPTIONS", "*", &[]).await.status.0, 200);
    }

    #[test]
    fn a_request_is_what_the_session_is_given() {
        // The driver hands the session what the message layer read, and
        // nothing else: no rewriting in between.
        let request = Request {
            method: Method::Options,
            uri: "*".to_owned(),
            headers: crate::rtsp::message::Headers::new().with("CSeq", "1"),
            body: Bytes::new(),
        };
        let mut session = Session::new();
        let actions = session.handle(&request, |_| None);
        assert!(matches!(actions[0], Action::Respond(_)));
    }
}
