//! The exchange that opens an RTMP connection, from the server's side.
//!
//! Six pieces in three round trips, named for who sends them:
//!
//! ```text
//! client ──C0──C1──▶            one version byte, then 1536 bytes
//!        ◀──S0──S1──S2──        the same, and the client's C1 back
//!        ──C2──▶                our S1 back
//! ```
//!
//! C1 and S1 are a timestamp, four bytes that are zero, and 1528 random
//! bytes. C2 and S2 echo the other side's: a timestamp, the time it was read
//! at, and the random bytes verbatim. Nothing is negotiated and nothing is
//! agreed — the chunk stream simply begins at the byte after C2.
//!
//! # What is not checked
//!
//! That C2 echoes S1. There is nothing to learn from it: the random bytes
//! went out in the clear, so anyone can echo them, and "the peer read what
//! we sent" is something TCP already says. Clients that get the echo wrong
//! exist, and refusing them would buy nothing. The bytes are counted and
//! dropped.
//!
//! # The handshake this does not do
//!
//! Flash Player 9 introduced a second form, where the four zero bytes hold a
//! client version instead and both sides embed an HMAC-SHA256 digest in
//! their random bytes. A client that tries it is answered plainly, which is
//! what every encoder in use accepts — the digest was only ever validated by
//! Flash Player itself. [`Handshake::client_version`] reports the attempt so
//! that a caller can say so.

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The only version of RTMP there is. Lower numbers are deprecated forms of
/// the protocol and higher ones were never defined.
pub const VERSION: u8 = 3;

/// How large C1, C2, S1 and S2 each are.
const HANDSHAKE_SIZE: usize = 1536;

/// How much of one is random: the rest is the two four-byte fields in front.
const RANDOM_SIZE: usize = HANDSHAKE_SIZE - 8;

/// What to do next.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Not enough has arrived. Read more into the same buffer and call
    /// again; nothing was consumed.
    NeedMore,
    /// Send these bytes, then call again.
    Send(Bytes),
    /// The handshake is over. Whatever is left in the buffer is the first of
    /// the chunk stream — a client may send it in the same packet as C2, so
    /// it has to be kept rather than discarded with the connection's
    /// handshake state.
    Done,
}

/// What can be wrong with a handshake.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandshakeError {
    /// The first byte a client sends names the protocol version, and 3 is
    /// the only one that is RTMP.
    #[error("the client asked for protocol version {0}, and only 3 is RTMP")]
    UnsupportedVersion(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Waiting for C0 and C1.
    Hello,
    /// Waiting for C2.
    Echo,
    /// Over.
    Done,
}

/// The server's half of the opening exchange.
///
/// Fed the bytes a client sends, in whatever pieces they arrive in, and asked
/// what to do next. It does no I/O of its own, so a caller reads into a
/// buffer, calls [`Handshake::read`], and writes whatever it is handed.
///
/// ```no_run
/// # use bytes::BytesMut;
/// # use relaybay::rtmp::handshake::{Handshake, Step};
/// # fn example(buf: &mut BytesMut) -> Result<(), Box<dyn std::error::Error>> {
/// let mut handshake = Handshake::new();
/// loop {
///     match handshake.read(buf)? {
///         Step::NeedMore => { /* read from the socket into `buf` */ }
///         Step::Send(reply) => { /* write `reply` to the socket */ }
///         Step::Done => break,
///     }
/// }
/// // `buf` may already hold the start of the chunk stream.
/// # Ok(())
/// # }
/// ```
pub struct Handshake {
    state: State,
    client_version: u32,
}

impl Default for Handshake {
    fn default() -> Self {
        Self::new()
    }
}

impl Handshake {
    /// A handshake that has seen nothing yet.
    pub fn new() -> Self {
        Self {
            state: State::Hello,
            client_version: 0,
        }
    }

    /// Whether the handshake is over, and the bytes after it are the chunk
    /// stream.
    pub fn is_done(&self) -> bool {
        self.state == State::Done
    }

    /// What the client put in the four bytes that a plain handshake leaves
    /// zero, which is its own version when it is attempting the digest form.
    ///
    /// `None` until C1 has been read, and `Some(0)` for the plain handshake
    /// every encoder in use performs. Anything else was answered plainly
    /// regardless — see the module docs — and is worth saying out loud only
    /// because it is the first thing to suspect if such a client then goes
    /// quiet.
    pub fn client_version(&self) -> Option<u32> {
        (self.state != State::Hello).then_some(self.client_version)
    }

    /// Takes what it can from `buf` and says what to do next.
    ///
    /// Consumes only whole pieces: a C1 that is one byte short leaves all of
    /// itself behind, so a caller can read more into the same buffer and
    /// call again.
    pub fn read(&mut self, buf: &mut BytesMut) -> Result<Step, HandshakeError> {
        match self.state {
            State::Hello => {
                // C0 and C1 are read together because a client sends them
                // together and there is nothing to do between them.
                if buf.len() < 1 + HANDSHAKE_SIZE {
                    return Ok(Step::NeedMore);
                }
                let version = buf[0];
                if version != VERSION {
                    return Err(HandshakeError::UnsupportedVersion(version));
                }
                buf.advance(1);
                let hello = buf.split_to(HANDSHAKE_SIZE);
                self.client_version = u32::from_be_bytes([hello[4], hello[5], hello[6], hello[7]]);
                self.state = State::Echo;
                Ok(Step::Send(reply(&hello)))
            }
            State::Echo => {
                if buf.len() < HANDSHAKE_SIZE {
                    return Ok(Step::NeedMore);
                }
                // C2, which is not checked — see the module docs.
                buf.advance(HANDSHAKE_SIZE);
                self.state = State::Done;
                Ok(Step::Done)
            }
            State::Done => Ok(Step::Done),
        }
    }
}

/// Builds S0, S1 and S2 in one piece, which is how a server sends them.
fn reply(hello: &[u8]) -> Bytes {
    let mut out = BytesMut::with_capacity(1 + HANDSHAKE_SIZE * 2);

    // S0.
    out.put_u8(VERSION);

    // S1: our own timestamp, which nothing reads, then the four bytes that
    // say this is a plain handshake, then random.
    out.put_u32(0);
    out.put_u32(0);
    let mut seed = seed(&hello[8..]);
    for _ in 0..RANDOM_SIZE / 8 {
        out.put_u64(scramble(&mut seed));
    }

    // S2: the client's C1 back. Its timestamp, then the time we read it at,
    // which the specification asks for and nothing reads, then its random
    // bytes unchanged.
    out.put_slice(&hello[..4]);
    out.put_u32(0);
    out.put_slice(&hello[8..]);

    out.freeze()
}

/// Mixes the client's own random bytes into a starting value.
///
/// Where S1's randomness comes from, and it is worth being plain about why
/// that is enough. The field is not a secret: it goes out in the clear and
/// comes straight back, nothing is derived from it, and no part of the
/// protocol after the handshake refers to it. All it has to be is not the
/// same on every connection, which taking it from what the client sent
/// achieves without a clock, a system call or a dependency.
fn seed(random: &[u8]) -> u64 {
    random.chunks(8).fold(0x9e37_79b9_7f4a_7c15, |seed, chunk| {
        let mut bytes = [0; 8];
        bytes[..chunk.len()].copy_from_slice(chunk);
        seed ^ u64::from_be_bytes(bytes).rotate_left(17)
    })
}

/// One step of splitmix64, which spreads a counter's bits about well enough
/// for a value nobody is trying to predict.
fn scramble(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut mixed = *state;
    mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    mixed ^ (mixed >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// C0 and C1, with `version` in the four bytes a plain handshake leaves
    /// zero and `random` repeated through the rest.
    fn hello(version: u32, random: u8) -> BytesMut {
        let mut out = BytesMut::new();
        out.put_u8(VERSION);
        out.put_u32(0x1122_3344);
        out.put_u32(version);
        out.put_slice(&[random; RANDOM_SIZE]);
        out
    }

    fn echo() -> BytesMut {
        BytesMut::from(&[0xab; HANDSHAKE_SIZE][..])
    }

    /// Drives a handshake to the end, returning what was sent and whatever
    /// was left in the buffer.
    fn run(buf: &mut BytesMut) -> (Bytes, Handshake) {
        let mut handshake = Handshake::new();
        let mut sent = Bytes::new();
        loop {
            match handshake.read(buf).expect("a well-formed handshake") {
                Step::NeedMore => panic!("everything is already in the buffer"),
                Step::Send(reply) => sent = reply,
                Step::Done => return (sent, handshake),
            }
        }
    }

    #[test]
    fn a_handshake_completes_and_answers_with_three_pieces() {
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        let (sent, handshake) = run(&mut buf);

        assert_eq!(sent.len(), 1 + HANDSHAKE_SIZE * 2);
        assert_eq!(sent[0], VERSION);
        assert!(handshake.is_done());
        assert!(buf.is_empty());
    }

    #[test]
    fn the_reply_sends_the_client_its_own_hello_back() {
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        let (sent, _) = run(&mut buf);

        let s2 = &sent[1 + HANDSHAKE_SIZE..];
        // The client's timestamp, unchanged.
        assert_eq!(&s2[..4], &0x1122_3344u32.to_be_bytes());
        // Its random bytes, unchanged.
        assert_eq!(&s2[8..], &[0x11; RANDOM_SIZE][..]);
    }

    #[test]
    fn the_reply_says_it_is_a_plain_handshake() {
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        let (sent, _) = run(&mut buf);

        let s1 = &sent[1..1 + HANDSHAKE_SIZE];
        assert_eq!(&s1[4..8], &[0, 0, 0, 0]);
    }

    #[test]
    fn what_we_send_is_not_what_we_were_sent() {
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        let (sent, _) = run(&mut buf);

        let s1_random = &sent[9..1 + HANDSHAKE_SIZE];
        assert_ne!(s1_random, &[0x11; RANDOM_SIZE][..]);
        // And it is not all one byte either, which a mixing step that did
        // nothing would leave it.
        assert!(s1_random.iter().any(|byte| *byte != s1_random[0]));
    }

    #[test]
    fn two_clients_are_not_sent_the_same_bytes() {
        let mut first = hello(0, 0x11);
        first.unsplit(echo());
        let mut second = hello(0, 0x12);
        second.unsplit(echo());

        assert_ne!(run(&mut first).0, run(&mut second).0);
    }

    #[test]
    fn nothing_is_consumed_until_a_whole_piece_is_there() {
        let mut whole = hello(0, 0x11);
        whole.unsplit(echo());

        // Feed it one byte at a time. Until a piece is complete every byte
        // of it has to still be in the buffer, or the one that arrives next
        // lands in the middle of something.
        let mut handshake = Handshake::new();
        let mut buf = BytesMut::new();
        let mut fed = 0;
        let mut sent = None;
        for byte in whole.iter() {
            buf.put_u8(*byte);
            fed += 1;
            match handshake.read(&mut buf).expect("well-formed") {
                Step::NeedMore => assert_eq!(buf.len(), fed),
                Step::Send(reply) => {
                    assert_eq!(fed, 1 + HANDSHAKE_SIZE);
                    sent = Some(reply);
                    fed = 0;
                }
                Step::Done => {
                    assert_eq!(fed, HANDSHAKE_SIZE);
                    fed = 0;
                }
            }
        }
        assert!(handshake.is_done());
        assert!(buf.is_empty());
        assert!(sent.is_some(), "the reply was never produced");
    }

    #[test]
    fn what_follows_the_handshake_is_left_where_it_is() {
        // A client is free to put its first chunks in the same packet as C2,
        // and dropping them with the handshake would lose its `connect`.
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        buf.put_slice(b"the chunk stream starts here");

        let (_, handshake) = run(&mut buf);
        assert!(handshake.is_done());
        assert_eq!(&buf[..], b"the chunk stream starts here");
    }

    #[test]
    fn a_version_that_is_not_rtmp_is_refused() {
        let mut buf = hello(0, 0x11);
        buf[0] = 6;
        assert_eq!(
            Handshake::new().read(&mut buf),
            Err(HandshakeError::UnsupportedVersion(6))
        );
    }

    #[test]
    fn a_client_reaching_for_the_digest_handshake_is_answered_plainly() {
        // What Flash Player put there: a version, rather than the zero a
        // plain handshake has.
        let mut buf = hello(0x8000_0702, 0x11);
        buf.unsplit(echo());

        let mut handshake = Handshake::new();
        assert_eq!(handshake.client_version(), None, "C1 has not been read");

        let Step::Send(sent) = handshake.read(&mut buf).expect("well-formed") else {
            panic!("C0 and C1 are both here");
        };
        assert_eq!(handshake.client_version(), Some(0x8000_0702));
        // Answered as a plain handshake all the same: S1's own four bytes
        // are zero, and no digest went anywhere.
        assert_eq!(&sent[5..9], &[0, 0, 0, 0]);
        assert_eq!(handshake.read(&mut buf), Ok(Step::Done));
    }

    #[test]
    fn reading_after_the_end_stays_at_the_end() {
        let mut buf = hello(0, 0x11);
        buf.unsplit(echo());
        let (_, mut handshake) = run(&mut buf);
        assert_eq!(handshake.read(&mut buf), Ok(Step::Done));
    }
}
