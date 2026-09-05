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

pub mod amf0;
pub mod chunk;
pub mod flv;
pub mod handshake;
pub mod server;
pub mod session;
