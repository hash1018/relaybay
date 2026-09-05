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

RTMP publishing works end to end: an encoder connects, and what it sends
comes out of a path as tracks and units. No egress exists yet, so nothing can
watch it.

Done:

- **Codecs** — H.264's two framings and conversion between them, the
  `AVCDecoderConfigurationRecord`, and AAC's `AudioSpecificConfig`
- **The common form** — tracks, descriptions and units
- **RTMP ingest** — handshake, chunk stream, AMF0, FLV tag bodies, and the
  session that turns all of it into a published stream
- **Paths** — a registry, a keyframe cache for readers that join late, and
  fan-out that drops rather than making a publisher wait

Planned, in order:

1. **RTSP egress** — SDP, and RTP packetization (FU-A, STAP-A)
2. **WebRTC egress** — via `str0m`
3. **HLS egress** — segments and a playlist

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

## Checking it against a real encoder

`examples/live.rs` starts a server, publishes to it with ffmpeg, and reads
what comes out. It needs ffmpeg on the path:

```
cargo run --example live
```

## Library or binary

`relaybay` is both. The binary reads a configuration and serves; the library is
the same server with the configuration left to the caller, so an application
that already has a media pipeline can embed it in-process instead of running a
second program and pushing to `localhost`.

## Licence

MIT or Apache-2.0, at your option.
