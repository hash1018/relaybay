//! Reading and writing the coded forms the protocols carry.
//!
//! Nothing here decodes anything. A relay only needs to know where one NAL
//! unit ends and the next begins, which of them a decoder must be
//! configured with, and how to write both facts back out in whatever shape
//! the next protocol expects.
//!
//! Each codec module has a `Parameters`: what a decoder must be given
//! before any of the stream means anything, and the whole of what a track's
//! description has to carry about it. Where a protocol wraps that in a
//! record of its own, the record is a separate type — the wrapper's fields
//! are about the wrapper, and an egress that frames some other way has no
//! use for them.

pub mod aac;
pub mod h264;
