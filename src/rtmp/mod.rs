//! RTMP: the protocol an encoder publishes with.
//!
//! One TCP connection carries audio, video and the commands that set a
//! session up, all interleaved. Reading it is a few layers, and this module
//! is one submodule per layer:
//!
//! - [`handshake`] opens the connection, and says where the rest begins.
//! - [`chunk`] cuts messages into pieces small enough to interleave, and
//!   puts them back together.
//! - [`amf0`] reads and writes the values a command message is made of.
//! - [`flv`] reads and writes what sits in front of the coded bytes in an
//!   audio or video message.
//! - [`session`] decides what all of that means: what to answer, what a
//!   publisher has said it is sending, and which units come out.
//!
//! None of those does any I/O. Each is fed what has arrived and asked what
//! it makes of it, so a whole publish can be driven in a test with no
//! runtime, and so the driver above can be replaced without any of them
//! changing. That boundary is the point: everything under it is protocol.
//!
//! [`server`] is the driver, and the only part of this module that has ever
//! seen a socket. It reads into a buffer, hands whole messages to a
//! [`session::Session`], and does what comes back.
//!
//! # How a publish runs
//!
//! [`crate::server::Server::start`] binds a port and spawns
//! [`server::serve`], which accepts into a [`tokio::task::JoinSet`] it owns
//! — so ending the listener ends every connection it took. Each of them runs
//! [`server::connection`], which is where the whole of the rest happens.
//!
//! ## Opening
//!
//! ```text
//! Handshake::new()
//! while !is_done() {
//!     handshake.read(&mut buf)
//!       NeedMore  → read from the socket into `buf`
//!       Send(rep) → write it
//!       Done      → break
//! }
//! ```
//!
//! Whatever followed C2 in the same packet is still in `buf`, and it is the
//! chunk stream. Discarding it would lose the `connect` that usually
//! arrives with it.
//!
//! ## The loop
//!
//! ```text
//! loop {
//!     while let Some(message) = reader.read(&mut buf)? {
//!         for action in session.handle(message)? {
//!             apply(action, writer, out, registry, publisher)?
//!         }
//!     }
//!     write everything built up in `out`
//!
//!     select! {
//!         read from the socket  → session.received(n)
//!         publisher.evicted()   → return
//!     }
//! }
//! ```
//!
//! [`chunk::Reader::read`] consumes only whole chunks, so it is called until
//! it says no before anything waits — one read can carry several messages,
//! and the last of them may be the one that matters.
//!
//! ## What each message turns into
//!
//! | Message | Handled by | Comes back as |
//! | ------- | ---------- | ------------- |
//! | `connect` | [`session::Session::handle`] | window size, peer bandwidth, chunk size, `_result` |
//! | `createStream` | the same | `_result(1)` |
//! | `publish` | the same | Stream Begin, `onStatus` |
//! | a sequence header | the same | nothing — it is kept |
//! | a frame | the same | [`session::Action::Publish`] the first time, then [`session::Action::Unit`] |
//!
//! The order inside one answer matters. [`session::Action::SetChunkSize`]
//! follows the message that announces it, because the peer goes on reading
//! at the old size until that message reaches it — and the announcement
//! itself would be the first thing chunked wrongly.
//!
//! ## Where a path comes from
//!
//! A sequence header is kept and answered with nothing. The description is
//! settled at the first frame instead, and only then does
//! [`crate::path::Registry::publish`] run and the path appear. Until that
//! moment a publisher has connected, been accepted, and produced nothing any
//! reader could be given.
//!
//! ```text
//! flv::read_video → SequenceHeader → h264::AvcConfig::parse
//!                                      ├─ parameters      → the description
//!                                      └─ nal_length_size → kept on the connection
//!
//!                 → Picture         → h264::split_length_prefixed
//!                                      └─ Vec<Nal>        → VideoUnit
//! ```
//!
//! The prefix width never reaches a track: it says how RTMP framed this
//! payload, not what the stream is.
//!
//! ## Ending
//!
//! Four ways, and all of them drop the [`crate::path::Publisher`]:
//! `deleteStream`, the socket closing, another publisher taking the path,
//! and a protocol error. Only the last is reported as one — the others are
//! how a publish is meant to finish.
//!
//! Dropping the publisher removes the path, but only if its token is still
//! the current one. A publisher that was displaced must not take its
//! replacement's stream down on the way out.

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;
pub mod server;
pub mod session;
