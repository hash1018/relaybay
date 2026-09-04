//! Reading and writing the coded forms the protocols carry.
//!
//! Nothing here decodes anything. A relay only needs to know where one NAL
//! unit ends and the next begins, which of them a decoder must be
//! configured with, and how to write both facts back out in whatever shape
//! the next protocol expects.

pub mod h264;
