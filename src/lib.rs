//! A media relay: one process that accepts a live stream over one protocol
//! and serves it over any of the others.
//!
//! # Why one process can do this
//!
//! Every protocol here carries the same thing — coded media and its timing —
//! and differs only in how it delimits it, how it transports it, and where
//! it puts the parameters a decoder needs to start. None of that touches the
//! coded bytes themselves, so a stream can change protocol without being
//! decoded and re-encoded. Relaying is repackaging.
//!
//! That is the whole architecture. An ingest parses its protocol down to
//! [`unit::Unit`]s and a [`track::Description`], a path fans those out, and
//! every egress packages them again for its own protocol. Nothing in the
//! middle knows which protocol anything came from.
//!
//! # The two things a path carries
//!
//! A [`track::Description`] says what the tracks are and what a decoder must
//! be given to start on each. Every protocol states it before it sends
//! anything — SDP, an `init.mp4`, an AVC sequence header — and they are all
//! the same facts in different notations, so it is kept as the facts.
//!
//! A [`unit::Unit`] is one access unit of that stream. Its payload carries
//! no framing at all: an H.264 unit holds NAL units as a list, with neither
//! Annex-B start codes nor length prefixes, and an AAC unit holds a raw
//! frame with no ADTS header. Framings belong to whoever is carrying the
//! media, so storing one would mean every other egress had to undo it first.
//! Each adds back only what its own protocol asks for.

pub mod codec;
pub mod path;
pub mod rtmp;
pub mod rtp;
pub mod server;
pub mod track;
pub mod unit;
