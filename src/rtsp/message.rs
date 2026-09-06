//! What goes back and forth on an RTSP connection.
//!
//! Two things share the socket, and telling them apart is the first thing
//! this does:
//!
//! ```text
//! DESCRIBE rtsp://host/live/cam1 RTSP/1.0\r\n   a request, in text
//! CSeq: 2\r\n
//! \r\n
//!
//! $ 00 05 dc  … 1500 bytes …                    media, in binary
//! ```
//!
//! The second form is how RTP travels when a client asked for it over the
//! same connection rather than over UDP. It begins with a dollar sign, which
//! no request line can, and says its own length — so a reader that finds one
//! skips exactly that far and keeps going.
//!
//! # Why this looks like HTTP and is not
//!
//! The syntax is HTTP's: a method, a URI, a version, then headers, then an
//! optional body counted by `Content-Length`. The differences are what stop
//! an HTTP server from being one of these. Requests go both ways, a session
//! spans many of them and is named in a header rather than a cookie, and the
//! binary frames above arrive between them on a connection an HTTP parser
//! would give up on.

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// How large the head of a request may be. Anything RTSP sends is a few
/// hundred bytes; the cap is so that a peer sending header bytes forever
/// does not make a server hold them.
pub const MAX_HEAD: usize = 8 * 1024;

/// How large a request body may be. Only `ANNOUNCE` and `SET_PARAMETER`
/// carry one, and an SDP is small.
pub const MAX_BODY: usize = 64 * 1024;

/// What a request asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
    /// What this server can do.
    Options,
    /// What the stream is, answered with SDP.
    Describe,
    /// Where to send one track, and how.
    Setup,
    /// Start sending.
    Play,
    /// Stop sending, without giving up the session.
    Pause,
    /// Give up the session.
    Teardown,
    /// A question about the session, and in practice a way of saying the
    /// client is still there.
    GetParameter,
    /// An answer to one, which nothing here has.
    SetParameter,
    /// A client describing a stream it is about to send.
    Announce,
    /// A client asking to send one.
    Record,
    /// Anything else, kept so that a reply can say what was refused.
    Other(String),
}

impl Method {
    fn parse(text: &str) -> Self {
        match text {
            "OPTIONS" => Self::Options,
            "DESCRIBE" => Self::Describe,
            "SETUP" => Self::Setup,
            "PLAY" => Self::Play,
            "PAUSE" => Self::Pause,
            "TEARDOWN" => Self::Teardown,
            "GET_PARAMETER" => Self::GetParameter,
            "SET_PARAMETER" => Self::SetParameter,
            "ANNOUNCE" => Self::Announce,
            "RECORD" => Self::Record,
            other => Self::Other(other.to_owned()),
        }
    }

    /// The word this is written as.
    pub fn name(&self) -> &str {
        match self {
            Self::Options => "OPTIONS",
            Self::Describe => "DESCRIBE",
            Self::Setup => "SETUP",
            Self::Play => "PLAY",
            Self::Pause => "PAUSE",
            Self::Teardown => "TEARDOWN",
            Self::GetParameter => "GET_PARAMETER",
            Self::SetParameter => "SET_PARAMETER",
            Self::Announce => "ANNOUNCE",
            Self::Record => "RECORD",
            Self::Other(name) => name,
        }
    }
}

/// A response's code and the words that go beside it.
///
/// The words are for a person reading a packet capture; no client reads
/// them, and every client reads the number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status(pub u16, pub &'static str);

impl Status {
    pub const OK: Self = Self(200, "OK");
    pub const BAD_REQUEST: Self = Self(400, "Bad Request");
    pub const NOT_FOUND: Self = Self(404, "Not Found");
    pub const METHOD_NOT_ALLOWED: Self = Self(405, "Method Not Allowed");
    pub const SESSION_NOT_FOUND: Self = Self(454, "Session Not Found");
    pub const METHOD_NOT_VALID: Self = Self(455, "Method Not Valid In This State");
    pub const UNSUPPORTED_TRANSPORT: Self = Self(461, "Unsupported Transport");
    pub const INTERNAL_ERROR: Self = Self(500, "Internal Server Error");
    pub const NOT_IMPLEMENTED: Self = Self(501, "Not Implemented");
}

/// A request's or a response's headers, in the order they were written.
///
/// Order is kept because a capture read by a person is easier to follow when
/// the answer's headers sit where the question's did. Lookup ignores case,
/// which the specification requires and clients rely on.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Headers(Vec<(String, String)>);

impl Headers {
    /// No headers.
    pub fn new() -> Self {
        Self::default()
    }

    /// The first value under `name`, whatever case it was written in.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// Adds one, keeping whatever is already there.
    pub fn push(&mut self, name: &str, value: impl Into<String>) {
        self.0.push((name.to_owned(), value.into()));
    }

    /// Adds one and hands the headers back, for building a response in one
    /// expression.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.push(name, value);
        self
    }

    /// Every header, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.0
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }
}

/// One request from a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub method: Method,
    pub uri: String,
    pub headers: Headers,
    pub body: Bytes,
}

impl Request {
    /// The sequence number this request has to be answered with.
    ///
    /// Every request carries one and every answer repeats it, which is how a
    /// client pairs them up on a connection where it may have several
    /// outstanding.
    pub fn sequence(&self) -> Option<&str> {
        self.headers.get("CSeq")
    }

    /// The session this belongs to, once one has been set up.
    ///
    /// Some clients write `Session: <id>;timeout=60`, so the part after the
    /// first semicolon is not the id.
    pub fn session(&self) -> Option<&str> {
        let value = self.headers.get("Session")?;
        Some(value.split(';').next().unwrap_or(value).trim())
    }
}

/// One answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub status: Status,
    pub headers: Headers,
    pub body: Bytes,
}

impl Response {
    /// An answer with a status and nothing else, which most of them are.
    pub fn new(status: Status) -> Self {
        Self {
            status,
            headers: Headers::new(),
            body: Bytes::new(),
        }
    }

    /// Adds a header.
    #[must_use]
    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push(name, value);
        self
    }

    /// Adds a body, and the type it is in.
    #[must_use]
    pub fn with_body(mut self, content_type: &str, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self.headers.push("Content-Type", content_type);
        self
    }

    /// Writes the answer out.
    ///
    /// `Content-Length` is written here rather than being asked for, because
    /// it is not a fact about the answer but about the bytes it is being
    /// turned into — and one that disagreed with them would leave a client
    /// reading the next response as part of this one.
    pub fn to_bytes(&self) -> Bytes {
        let mut out = BytesMut::new();
        out.put_slice(format!("RTSP/1.0 {} {}\r\n", self.status.0, self.status.1).as_bytes());
        for (name, value) in self.headers.iter() {
            out.put_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        out.put_slice(format!("Content-Length: {}\r\n\r\n", self.body.len()).as_bytes());
        out.put_slice(&self.body);
        out.freeze()
    }
}

/// What arrived on the connection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Incoming {
    /// A request, in text.
    Request(Request),
    /// A frame of RTP or RTCP, on the channel a `SETUP` agreed.
    Interleaved { channel: u8, data: Bytes },
}

/// What can be wrong with what arrived.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RtspError {
    /// A first line that is not a method, a URI and a version.
    #[error("a request line this cannot read: {0:?}")]
    MalformedRequestLine(String),

    /// A header line with no colon in it.
    #[error("a header line with no name: {0:?}")]
    MalformedHeader(String),

    /// A version this does not speak. There has only ever been 1.0.
    #[error("RTSP version {0:?}, and only 1.0 exists")]
    UnsupportedVersion(String),

    /// A `Content-Length` that is not a number.
    #[error("a content length this cannot read: {0:?}")]
    MalformedContentLength(String),

    /// A request head longer than [`MAX_HEAD`], or a body longer than
    /// [`MAX_BODY`].
    #[error("a {part} of more than {limit} bytes")]
    TooLong { part: &'static str, limit: usize },

    /// Bytes that are neither a request nor a frame. Refused rather than
    /// skipped past: a connection that is out of step does not come back
    /// into it, and reading on would turn one client's confusion into a
    /// stream of nonsense.
    #[error("neither a request nor an interleaved frame")]
    NotRtsp,
}

/// Takes the next thing out of `buf`, or `None` if there is not one there
/// yet.
///
/// Consumes only what it returns. A request that is half here leaves all of
/// itself behind, so a caller reads more into the same buffer and calls
/// again.
pub fn read(buf: &mut BytesMut) -> Result<Option<Incoming>, RtspError> {
    match buf.first() {
        None => Ok(None),
        // A request line starts with a method, and a method is letters.
        Some(b'$') => read_interleaved(buf),
        Some(byte) if byte.is_ascii_alphabetic() => read_request(buf),
        Some(_) => Err(RtspError::NotRtsp),
    }
}

fn read_interleaved(buf: &mut BytesMut) -> Result<Option<Incoming>, RtspError> {
    let Some(head) = buf.get(..4) else {
        return Ok(None);
    };
    let channel = head[1];
    let length = usize::from(u16::from_be_bytes([head[2], head[3]]));
    if buf.len() < 4 + length {
        return Ok(None);
    }
    buf.advance(4);
    Ok(Some(Incoming::Interleaved {
        channel,
        data: buf.split_to(length).freeze(),
    }))
}

fn read_request(buf: &mut BytesMut) -> Result<Option<Incoming>, RtspError> {
    let Some(end) = find(buf, b"\r\n\r\n") else {
        if buf.len() > MAX_HEAD {
            return Err(RtspError::TooLong {
                part: "request head",
                limit: MAX_HEAD,
            });
        }
        return Ok(None);
    };
    // The head, without the blank line that ends it.
    let head = std::str::from_utf8(&buf[..end]).map_err(|_| RtspError::NotRtsp)?;
    let mut lines = head.split("\r\n");

    let line = lines.next().unwrap_or_default();
    let mut parts = line.split_whitespace();
    let (Some(method), Some(uri), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return Err(RtspError::MalformedRequestLine(line.to_owned()));
    };
    if version != "RTSP/1.0" {
        return Err(RtspError::UnsupportedVersion(version.to_owned()));
    }

    let mut headers = Headers::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(RtspError::MalformedHeader(line.to_owned()));
        };
        headers.push(name.trim(), value.trim());
    }

    let length = match headers.get("Content-Length") {
        None => 0,
        Some(stated) => stated
            .trim()
            .parse::<usize>()
            .map_err(|_| RtspError::MalformedContentLength(stated.to_owned()))?,
    };
    if length > MAX_BODY {
        return Err(RtspError::TooLong {
            part: "request body",
            limit: MAX_BODY,
        });
    }
    let head_len = end + 4;
    if buf.len() < head_len + length {
        return Ok(None);
    }

    let method = Method::parse(method);
    let uri = uri.to_owned();
    buf.advance(head_len);
    Ok(Some(Incoming::Request(Request {
        method,
        uri,
        headers,
        body: buf.split_to(length).freeze(),
    })))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Wraps a packet in the four bytes that let it share the connection with
/// requests. See the module docs.
///
/// Fails for a packet too long for the two bytes that count it, which no
/// packetizer here produces.
pub fn interleave(out: &mut BytesMut, channel: u8, packet: &[u8]) -> Result<(), RtspError> {
    let length = u16::try_from(packet.len()).map_err(|_| RtspError::TooLong {
        part: "interleaved frame",
        limit: usize::from(u16::MAX),
    })?;
    out.put_u8(b'$');
    out.put_u8(channel);
    out.put_u16(length);
    out.put_slice(packet);
    Ok(())
}

/// Where a client wants one track's packets sent, and how.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    /// On this connection, as interleaved frames. Two channels: the packets
    /// on the first, the reports about them on the second.
    Interleaved { rtp: u8, rtcp: u8 },
    /// To a pair of ports over UDP.
    Udp { rtp: u16, rtcp: u16 },
}

impl Transport {
    /// Reads a `Transport` header, taking the first form it offers that this
    /// can express.
    ///
    /// A client lists what it will accept, in the order it would prefer, so
    /// refusing the whole header because one of them is multicast would turn
    /// down an offer that also included something workable.
    pub fn parse(header: &str) -> Option<Self> {
        Self::offered(header).next()
    }

    /// Every form the header offers that this can express, in the order the
    /// client would prefer them.
    ///
    /// What a caller that can do some of them and not others reads instead
    /// of [`Transport::parse`]: which ones those are is a fact about the
    /// server rather than about the header.
    pub fn offered(header: &str) -> impl Iterator<Item = Self> + '_ {
        header.split(',').filter_map(Self::parse_one)
    }

    fn parse_one(spec: &str) -> Option<Self> {
        let mut parts = spec.split(';').map(str::trim);
        let protocol = parts.next()?;
        if !protocol.starts_with("RTP/AVP") {
            return None;
        }
        let interleaved = protocol.ends_with("/TCP");
        let mut ports = None;
        let mut channels = None;
        let mut multicast = false;
        for part in parts {
            match part.split_once('=') {
                Some(("interleaved", value)) => channels = pair(value),
                Some(("client_port", value)) => ports = pair(value),
                _ => multicast |= part == "multicast",
            }
        }
        // Every reader here is one client on one connection. Multicast is a
        // different shape of stream, not a different address for this one.
        if multicast {
            return None;
        }
        if interleaved {
            let (rtp, rtcp) = channels?;
            Some(Self::Interleaved {
                rtp: u8::try_from(rtp).ok()?,
                rtcp: u8::try_from(rtcp).ok()?,
            })
        } else {
            let (rtp, rtcp) = ports?;
            Some(Self::Udp { rtp, rtcp })
        }
    }

    /// Writes the header a `SETUP` is answered with.
    pub fn to_header(self, server: Option<(u16, u16)>) -> String {
        match self {
            Self::Interleaved { rtp, rtcp } => {
                format!("RTP/AVP/TCP;unicast;interleaved={rtp}-{rtcp}")
            }
            Self::Udp { rtp, rtcp } => {
                let mut header = format!("RTP/AVP;unicast;client_port={rtp}-{rtcp}");
                if let Some((rtp, rtcp)) = server {
                    header.push_str(&format!(";server_port={rtp}-{rtcp}"));
                }
                header
            }
        }
    }
}

/// Reads `a-b`, which is how both channels and ports are written.
fn pair(value: &str) -> Option<(u16, u16)> {
    let (first, second) = value.split_once('-')?;
    Some((first.trim().parse().ok()?, second.trim().parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(text: &str) -> BytesMut {
        BytesMut::from(text.as_bytes())
    }

    fn request(buf: &mut BytesMut) -> Request {
        match read(buf).unwrap() {
            Some(Incoming::Request(request)) => request,
            other => panic!("a request, got {other:?}"),
        }
    }

    #[test]
    fn a_request_reads_as_its_parts() {
        let mut buf = buffer(
            "DESCRIBE rtsp://host/live/cam1 RTSP/1.0\r\n\
             CSeq: 2\r\n\
             Accept: application/sdp\r\n\
             User-Agent: LibVLC/3.0\r\n\
             \r\n",
        );
        let request = request(&mut buf);

        assert_eq!(request.method, Method::Describe);
        assert_eq!(request.uri, "rtsp://host/live/cam1");
        assert_eq!(request.sequence(), Some("2"));
        assert_eq!(request.headers.get("Accept"), Some("application/sdp"));
        assert!(request.body.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn a_header_is_found_whatever_case_it_was_written_in() {
        let mut buf = buffer("OPTIONS * RTSP/1.0\r\ncseq: 1\r\nSESSION: abc\r\n\r\n");
        let request = request(&mut buf);
        assert_eq!(request.sequence(), Some("1"));
        assert_eq!(request.session(), Some("abc"));
    }

    #[test]
    fn a_session_id_is_not_the_timeout_beside_it() {
        let mut buf = buffer("PLAY rtsp://h/s RTSP/1.0\r\nSession: 1234abcd;timeout=60\r\n\r\n");
        assert_eq!(request(&mut buf).session(), Some("1234abcd"));
    }

    #[test]
    fn a_request_with_a_body_waits_for_all_of_it() {
        let head = "ANNOUNCE rtsp://h/s RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 10\r\n\r\n";
        let mut buf = buffer(head);
        assert_eq!(read(&mut buf).unwrap(), None, "the body has not arrived");
        assert_eq!(buf.len(), head.len(), "and nothing was consumed");

        buf.put_slice(b"v=0\r\ns=x\r\n");
        let request = request(&mut buf);
        assert_eq!(&request.body[..], b"v=0\r\ns=x\r\n");
        assert!(buf.is_empty());
    }

    #[test]
    fn a_request_arriving_a_byte_at_a_time_reads_the_same() {
        let whole = "SETUP rtsp://h/s/trackID=0 RTSP/1.0\r\n\
                     CSeq: 3\r\n\
                     Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\
                     \r\n";
        let mut buf = BytesMut::new();
        for (index, byte) in whole.bytes().enumerate() {
            buf.put_u8(byte);
            let read = read(&mut buf).unwrap();
            if index + 1 < whole.len() {
                assert_eq!(read, None, "at {index}");
                assert_eq!(buf.len(), index + 1, "nothing consumed at {index}");
            } else {
                assert!(matches!(read, Some(Incoming::Request(_))));
                assert!(buf.is_empty());
            }
        }
    }

    #[test]
    fn two_requests_in_one_read_both_come_out() {
        let mut buf = buffer(
            "OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\nDESCRIBE rtsp://h/s RTSP/1.0\r\nCSeq: 2\r\n\r\n",
        );
        assert_eq!(request(&mut buf).method, Method::Options);
        assert_eq!(request(&mut buf).method, Method::Describe);
        assert_eq!(read(&mut buf).unwrap(), None);
    }

    #[test]
    fn a_frame_of_media_between_requests_is_read_as_one() {
        let mut buf = BytesMut::new();
        interleave(&mut buf, 1, &[0x80, 0xc9, 0x00, 0x01]).unwrap();
        buf.put_slice(b"OPTIONS * RTSP/1.0\r\nCSeq: 9\r\n\r\n");

        assert_eq!(
            read(&mut buf).unwrap(),
            Some(Incoming::Interleaved {
                channel: 1,
                data: Bytes::from_static(&[0x80, 0xc9, 0x00, 0x01])
            })
        );
        assert_eq!(request(&mut buf).sequence(), Some("9"));
    }

    #[test]
    fn a_frame_that_is_half_here_consumes_nothing() {
        let mut whole = BytesMut::new();
        interleave(&mut whole, 0, &[0xab; 200]).unwrap();
        for cut in 0..whole.len() {
            let mut buf = whole.clone().split_to(cut);
            assert_eq!(read(&mut buf).unwrap(), None, "{cut}");
            assert_eq!(buf.len(), cut, "{cut}");
        }
    }

    #[test]
    fn a_response_writes_its_own_content_length() {
        let response = Response::new(Status::OK)
            .with("CSeq", "2")
            .with_body("application/sdp", Bytes::from_static(b"v=0\r\n"));
        let written = String::from_utf8(response.to_bytes().to_vec()).unwrap();

        assert_eq!(
            written,
            "RTSP/1.0 200 OK\r\n\
             CSeq: 2\r\n\
             Content-Type: application/sdp\r\n\
             Content-Length: 5\r\n\
             \r\n\
             v=0\r\n"
        );
    }

    #[test]
    fn a_response_with_no_body_still_says_so() {
        // A client that read no length would wait for a body that is not
        // coming.
        let written = Response::new(Status::OK).with("CSeq", "1").to_bytes();
        assert!(String::from_utf8_lossy(&written).contains("Content-Length: 0"));
    }

    #[test]
    fn a_transport_over_the_connection_reads_as_its_channels() {
        assert_eq!(
            Transport::parse("RTP/AVP/TCP;unicast;interleaved=0-1"),
            Some(Transport::Interleaved { rtp: 0, rtcp: 1 })
        );
        assert_eq!(
            Transport::parse("RTP/AVP/TCP;interleaved=4-5;unicast"),
            Some(Transport::Interleaved { rtp: 4, rtcp: 5 })
        );
    }

    #[test]
    fn a_transport_over_udp_reads_as_its_ports() {
        assert_eq!(
            Transport::parse("RTP/AVP;unicast;client_port=8000-8001"),
            Some(Transport::Udp {
                rtp: 8000,
                rtcp: 8001
            })
        );
        assert_eq!(
            Transport::parse("RTP/AVP/UDP;unicast;client_port=50000-50001;mode=play"),
            Some(Transport::Udp {
                rtp: 50000,
                rtcp: 50001
            })
        );
    }

    #[test]
    fn the_first_thing_offered_that_works_is_taken() {
        // A client lists what it will accept, best first. Turning the whole
        // header down over one entry would refuse an offer that had
        // something workable in it.
        assert_eq!(
            Transport::parse(
                "RTP/AVP;multicast;port=5000-5001,RTP/AVP/TCP;unicast;interleaved=0-1"
            ),
            Some(Transport::Interleaved { rtp: 0, rtcp: 1 })
        );
    }

    #[test]
    fn a_transport_this_cannot_do_reads_as_nothing() {
        assert_eq!(Transport::parse("RTP/AVP;multicast;port=5000-5001"), None);
        assert_eq!(Transport::parse("RAW/RAW/UDP;unicast;port=5000"), None);
        // Interleaved without the channels to interleave on.
        assert_eq!(Transport::parse("RTP/AVP/TCP;unicast"), None);
    }

    #[test]
    fn a_transport_header_round_trips() {
        let interleaved = Transport::Interleaved { rtp: 2, rtcp: 3 };
        assert_eq!(
            Transport::parse(&interleaved.to_header(None)),
            Some(interleaved)
        );

        let udp = Transport::Udp {
            rtp: 8000,
            rtcp: 8001,
        };
        let header = udp.to_header(Some((9000, 9001)));
        assert!(header.contains("server_port=9000-9001"));
        assert_eq!(Transport::parse(&header), Some(udp));
    }

    #[test]
    fn a_request_line_that_is_not_one_is_refused() {
        let mut buf = buffer("DESCRIBE\r\nCSeq: 1\r\n\r\n");
        assert_eq!(
            read(&mut buf),
            Err(RtspError::MalformedRequestLine("DESCRIBE".to_owned()))
        );
    }

    #[test]
    fn a_version_that_does_not_exist_is_refused() {
        let mut buf = buffer("OPTIONS * RTSP/2.0\r\nCSeq: 1\r\n\r\n");
        assert_eq!(
            read(&mut buf),
            Err(RtspError::UnsupportedVersion("RTSP/2.0".to_owned()))
        );
    }

    #[test]
    fn a_header_with_no_name_is_refused() {
        let mut buf = buffer("OPTIONS * RTSP/1.0\r\nnonsense\r\n\r\n");
        assert_eq!(
            read(&mut buf),
            Err(RtspError::MalformedHeader("nonsense".to_owned()))
        );
    }

    #[test]
    fn bytes_that_are_neither_are_refused_rather_than_skipped() {
        // A connection out of step does not come back into it, and reading
        // on would turn one client's confusion into a stream of nonsense.
        let mut buf = buffer("\x01\x02\x03");
        assert_eq!(read(&mut buf), Err(RtspError::NotRtsp));
    }

    #[test]
    fn a_head_that_never_ends_is_refused() {
        let mut buf = BytesMut::new();
        buf.put_slice(b"OPTIONS * RTSP/1.0\r\n");
        buf.put_slice(&vec![b'x'; MAX_HEAD]);
        assert_eq!(
            read(&mut buf),
            Err(RtspError::TooLong {
                part: "request head",
                limit: MAX_HEAD
            })
        );
    }

    #[test]
    fn a_body_larger_than_anything_rtsp_sends_is_refused() {
        let mut buf = buffer(&format!(
            "ANNOUNCE rtsp://h/s RTSP/1.0\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY + 1
        ));
        assert_eq!(
            read(&mut buf),
            Err(RtspError::TooLong {
                part: "request body",
                limit: MAX_BODY
            })
        );
    }

    #[test]
    fn a_method_round_trips_through_its_name() {
        for method in [
            Method::Options,
            Method::Describe,
            Method::Setup,
            Method::Play,
            Method::Pause,
            Method::Teardown,
            Method::GetParameter,
            Method::SetParameter,
            Method::Announce,
            Method::Record,
            Method::Other("REDIRECT".to_owned()),
        ] {
            assert_eq!(Method::parse(method.name()), method);
        }
    }
}
