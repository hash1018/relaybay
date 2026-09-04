//! The standalone relay.
//!
//! Everything it does lives in the library, so an application that already
//! has a media pipeline can embed the same server rather than run this.

fn main() {
    println!("relaybay {}", env!("CARGO_PKG_VERSION"));
}
