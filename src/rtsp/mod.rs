//! RTSP: the protocol a player asks for a stream with.
//!
//! It carries no media of its own. What it does is agree on what a stream
//! is and where its packets should go, and then the packets go as RTP —
//! either to a pair of UDP ports the client named, or back down the same
//! connection wrapped in four bytes.
//!
//! ```text
//! OPTIONS   ──▶  what this server can do
//! DESCRIBE  ──▶  the SDP: what the tracks are, and how to decode them
//! SETUP     ──▶  once per track: where to send it
//! PLAY      ──▶  start
//!               ◀── RTP, on the transport SETUP agreed
//! TEARDOWN  ──▶  stop
//! ```
//!
//! - [`message`] reads requests and writes answers, and tells both apart
//!   from the media frames that share the connection.
//! - [`sdp`] writes a [`crate::track::Description`] as the text `DESCRIBE`
//!   is answered with.
//!
//! Neither does any I/O, which is the same division the RTMP side has:
//! everything under the driver is fed what arrived and asked what it makes
//! of it.

pub mod message;
pub mod sdp;
pub mod session;
