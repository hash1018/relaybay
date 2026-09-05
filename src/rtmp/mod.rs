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
//!
//! None of them does any I/O: each is fed a buffer and asked what it makes
//! of it, so that the socket, and the choice of runtime it implies, stays at
//! the edge.
//!
//! Nothing here is specific to a codec. What arrives is a message with a
//! type, a timestamp and a payload; turning the video ones into
//! [`crate::unit::VideoUnit`]s is the ingest's job, and it uses
//! [`crate::codec`] to do it.

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;
