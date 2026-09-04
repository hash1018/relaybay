//! A media relay: one process that accepts a live stream over one protocol
//! and serves it over any of the others.
//!
//! # Why one process can do this
//!
//! Every protocol here carries the same thing — coded pictures and their
//! timing — and differs only in how it delimits them, how it transports
//! them, and where it puts the parameters a decoder needs to start. None of
//! that touches the coded bytes themselves, so a stream can change protocol
//! without being decoded and re-encoded. Relaying is repackaging.
//!
//! That is the whole architecture. An ingest parses its protocol down to
//! [`unit::VideoUnit`]s, a path fans those out, and every egress packages
//! them again for its own protocol. Nothing in the middle knows which
//! protocol anything came from.
//!
//! # What the common form is
//!
//! A [`unit::VideoUnit`] holds NAL units as a list, with no framing at all —
//! neither Annex-B start codes nor length prefixes. Both of those are
//! framings *of* a NAL unit stream, chosen by whoever is carrying it, so
//! storing either would mean every other egress had to undo it first. A list
//! is what they have in common, and each egress adds back only what its own
//! protocol asks for.

pub mod codec;
pub mod unit;
