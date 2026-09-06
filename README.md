# relaybay

A media relay: one process that accepts a live stream over one protocol and
serves it over any of the others.

```
OBS ──RTMP──▶ relaybay ──RTSP───▶ VLC
                       ──HLS────▶ browser
                       ──WebRTC─▶ browser
```

## Why one process can do this

Every protocol here carries the same thing — coded pictures and their timing —
and differs only in three ways:

|          | NAL unit boundaries | Parameter sets (SPS/PPS) | Transport   |
| -------- | ------------------- | ------------------------ | ----------- |
| RTMP     | length prefix       | AVC sequence header      | TCP         |
| RTSP     | the RTP header      | SDP                      | TCP + UDP   |
| HLS      | start codes or length prefix | segment or init.mp4 | HTTP |
| WebRTC   | the RTP header      | in front of every keyframe | SRTP/ICE  |

None of that touches the coded bytes. A picture that arrived over RTMP leaves
over RTSP as the same bytes in a different wrapper, so relaying is repackaging
and never decoding. That is why one small binary can serve all of them at once,
and why it costs almost no CPU to do so.

The limit is the codec, not the protocol: repackaging is free, transcoding is
not. A stream can change protocol freely as long as its codec is one both ends
carry — which is why AAC audio, universal over RTMP and RTSP, cannot reach a
WebRTC reader that only takes Opus.

## Shape

```
ingest ──▶ path ──▶ egress
           │
           └─ one publisher, any number of readers, each on its own protocol
```

A path carries two things. A `Description` says what the tracks are and what
a decoder must be given to start on each — every protocol states that before
it sends anything, as SDP or an `init.mp4` or an AVC sequence header, and all
of them are the same facts in different notations.

The media itself is a run of `Unit`s, and a unit's payload carries no framing
at all: H.264 as a list of NAL units, with neither Annex-B start codes nor
length prefixes, and AAC as a raw frame with no ADTS header. Framings belong
to whoever is carrying the media, so storing one would mean every other
egress had to undo it first. Each adds back only what its own protocol asks
for.

## Status

It relays. An encoder publishes over RTMP and a player watches over RTSP:

```
ffmpeg ──RTMP──▶ relaybay ──RTSP──▶ ffmpeg / VLC
```

Done:

- **Codecs** — H.264's two framings and conversion between them, the
  `AVCDecoderConfigurationRecord`, and AAC's `AudioSpecificConfig`
- **The common form** — tracks, descriptions and units
- **RTMP ingest** — handshake, chunk stream, AMF0, FLV tag bodies, and the
  session that turns all of it into a published stream
- **Paths** — a registry, a keyframe cache for readers that join late, and
  fan-out that drops rather than making a publisher wait
- **RTP** — H.264 as single packets and FU-A fragments, AAC with the AU
  headers RFC 3640 asks for
- **RTSP egress** — `DESCRIBE`, `SETUP`, `PLAY`, and the packets themselves
  interleaved on the same connection

Planned, in order:

1. **RTSP over UDP** — for clients that will not take the interleaved form
2. **RTCP** — sender reports, which is what keeps audio and video in step
   over a long stream
3. **WebRTC egress** — via `str0m`
4. **HLS egress** — segments and a playlist

[`ARCHITECTURE.md`](ARCHITECTURE.md) has the rest: what each module does, how
a publish and a play run end to end, and what every layer refuses.

## Where the runtime is

Everything that reads or writes a socket uses `tokio`. Nothing under that
does: codecs, chunks, AMF0 and the session state machine are fed a buffer and
asked what they make of it, so the whole of a publish is driven in tests with
no runtime at all.

That boundary is also what makes embedding work. `Server::start` builds a
runtime and keeps it inside the handle it returns, so an application on
ordinary threads never sees one:

```rust
let server = relaybay::server::Server::start(Default::default())?;
// … the application's own threads run as they always did …
server.shutdown();
```

An application that already has a runtime uses `Server::start_on` instead,
and gets no second set of worker threads.

## Checking it against real software

Tests written against this crate's own code can be self-consistently wrong,
so two examples put ffmpeg on the far side. Both need it on the path.

`examples/live.rs` publishes with ffmpeg and reads the units back in
process — it checks that a real encoder is understood:

```
cargo run --example live
```

`examples/relay.rs` publishes with ffmpeg over RTMP, plays with ffmpeg over
RTSP, and probes the file that comes out. Neither end has seen this code:

```
cargo run --example relay
```

## Library or binary

`relaybay` is both. The binary reads a configuration and serves; the library is
the same server with the configuration left to the caller, so an application
that already has a media pipeline can embed it in-process instead of running a
second program and pushing to `localhost`.

## Licence

MIT or Apache-2.0, at your option.
