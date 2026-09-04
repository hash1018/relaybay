//! RTMP: the protocol an encoder publishes with.
//!
//! One TCP connection carries audio, video and the commands that set a
//! session up, all interleaved. Reading it is three layers, and this module
//! is one submodule per layer:
//!
//! - [`chunk`] cuts messages into pieces small enough to interleave, and
//!   puts them back together.
//!
//! Nothing here is specific to a codec. What arrives is a message with a
//! type, a timestamp and a payload; turning the video ones into
//! [`crate::unit::VideoUnit`]s is the ingest's job, and it uses
//! [`crate::codec`] to do it.

pub mod chunk;
