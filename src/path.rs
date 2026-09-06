//! Where a stream is put, and where readers find it.
//!
//! A path is a name with one publisher and any number of readers. The
//! publisher hands it units; every reader gets each of them. Nothing here
//! knows a protocol: an RTMP ingest and an RTSP one publish the same way,
//! and an RTSP reader and a WebRTC one read the same way.
//!
//! # A publisher is never made to wait
//!
//! [`Publisher::push`] does not block and cannot fail. A live encoder has
//! nowhere to put a picture it is not allowed to send, so pushing back on it
//! does not slow anything down — it breaks the broadcast. Every queue in
//! here is bounded and drops instead, and the reader that fell behind is the
//! one that loses something.
//!
//! # What a reader that fell behind loses
//!
//! Not the oldest units, but everything up to the next picture it can start
//! at. Dropping one picture out of the middle of a group leaves the ones
//! after it referring to something the reader never got: they decode into
//! smears rather than into nothing, which is worse, and they go on doing it
//! until the next keyframe anyway. So the skip is made deliberately and all
//! at once.
//!
//! # What a reader gets before anything live
//!
//! The units since the last picture a stream can be started at, so a reader
//! that joins between keyframes has something to decode immediately instead
//! of a black screen until the next one — at a two second keyframe interval,
//! a second of nothing on average.
//!
//! One group of pictures, and never part of one. A group that grows past
//! [`CACHE_LIMIT`] is dropped whole and nothing is kept until the next
//! keyframe, because half a group would not start a reader anyway. A stream
//! with no pictures has nothing to cut a group at, so it drops its oldest
//! instead, which is right where every frame stands alone.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{broadcast, oneshot};

use crate::track::Description;
use crate::unit::Unit;

/// How many units a reader may fall behind by before it is made to skip.
///
/// Counted in units rather than bytes because that is what the queue under
/// it counts. Several seconds of a stream at any ordinary frame rate, which
/// is far longer than a reader that is going to recover needs.
pub const BACKLOG: usize = 1024;

/// How much of a stream is held for readers that have not joined yet.
///
/// A cap rather than a target: what is kept is one group of pictures, and
/// this is only what happens when a publisher sends one longer than any
/// reader would want to be handed at once.
pub const CACHE_LIMIT: usize = 8 * 1024 * 1024;

/// Every path, by name.
///
/// The lock is a plain [`RwLock`] and is only ever held long enough to find
/// or replace an [`Arc`]. Nothing is sent while it is held, so a slow reader
/// cannot stop a publisher from finding its path.
#[derive(Default)]
pub struct Registry {
    paths: RwLock<HashMap<String, Arc<Path>>>,
    published: AtomicU64,
}

impl Registry {
    /// An empty registry.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes over `name`, and says what to feed it with.
    ///
    /// Cannot fail. A second publisher on a name displaces the first, which
    /// learns of it through [`Publisher::evicted`] — an encoder that lost
    /// its network and is reconnecting looks exactly like one that never
    /// left, and refusing the new one would leave the path stuck behind a
    /// connection that is not going to send anything again.
    ///
    /// Readers on the old stream are ended rather than moved across. The new
    /// publisher has its own description, and a reader handed units that do
    /// not match the one it was set up with would decode nonsense.
    pub fn publish(self: &Arc<Self>, name: &str, description: Description) -> Publisher {
        let token = self.published.fetch_add(1, Ordering::Relaxed);
        let (evict, evicted) = oneshot::channel();
        let (feed, _) = broadcast::channel(BACKLOG);
        let source = Source {
            token,
            description: Arc::new(description),
            feed,
            cache: Cache::new(),
            _evict: evict,
        };

        let path = self
            .paths
            .write()
            .expect("no panic holds this")
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Path::default()))
            .clone();
        // Replacing the source drops the old one, which closes its feed and
        // its eviction channel: readers see the stream end and the previous
        // publisher wakes up.
        *path.source.lock().expect("no panic holds this") = Some(source);

        Publisher {
            registry: Arc::clone(self),
            name: name.to_owned(),
            path,
            token,
            evicted,
        }
    }

    /// Attaches to whatever is being published to `name`, or `None` if
    /// nothing is.
    pub fn read(&self, name: &str) -> Option<Reader> {
        let path = self
            .paths
            .read()
            .expect("no panic holds this")
            .get(name)
            .cloned()?;
        // The backlog and the subscription are taken under one lock. A
        // publisher takes the same lock to push, so there is no instant
        // between them in which a unit could be sent and belong to neither.
        let source = path.source.lock().expect("no panic holds this");
        let source = source.as_ref()?;
        Some(Reader {
            description: Arc::clone(&source.description),
            backlog: source.cache.units.iter().cloned().collect(),
            feed: source.feed.subscribe(),
            skipped: 0,
        })
    }

    /// What is being published to `name`, without attaching to it.
    ///
    /// Separate from [`Registry::read`] because attaching copies the
    /// keyframe a reader would start on, and a question about what a stream
    /// is — an RTSP `DESCRIBE`, a listing — is not a reason to copy a
    /// megabyte.
    pub fn describe(&self, name: &str) -> Option<Arc<Description>> {
        let path = self
            .paths
            .read()
            .expect("no panic holds this")
            .get(name)
            .cloned()?;
        let source = path.source.lock().expect("no panic holds this");
        source
            .as_ref()
            .map(|source| Arc::clone(&source.description))
    }

    /// The names something is being published to, in no particular order.
    pub fn names(&self) -> Vec<String> {
        self.paths
            .read()
            .expect("no panic holds this")
            .iter()
            .filter(|(_, path)| path.source.lock().expect("no panic holds this").is_some())
            .map(|(name, _)| name.clone())
            .collect()
    }
}

/// One name, which may or may not have something being published to it.
#[derive(Default)]
struct Path {
    source: Mutex<Option<Source>>,
}

/// What is being published to a path right now.
struct Source {
    /// Which publisher this is, so that one that has been displaced can tell
    /// without having to compare anything larger.
    token: u64,
    description: Arc<Description>,
    feed: broadcast::Sender<Unit>,
    cache: Cache,
    /// Held only to be dropped: dropping it is what tells the publisher it
    /// has been displaced.
    _evict: oneshot::Sender<()>,
}

/// The right to publish to a path.
///
/// Dropping it takes the stream down, and readers see it end.
pub struct Publisher {
    registry: Arc<Registry>,
    name: String,
    path: Arc<Path>,
    token: u64,
    evicted: oneshot::Receiver<()>,
}

impl Publisher {
    /// Hands a unit to every reader.
    ///
    /// Never blocks and never fails. A path with no readers, a reader too
    /// far behind, a publisher that has been displaced — none of them is
    /// something an encoder can do anything about, so none of them is
    /// reported here. See the module docs.
    pub fn push(&self, unit: Unit) {
        let mut source = self.path.source.lock().expect("no panic holds this");
        let Some(source) = source.as_mut() else {
            return;
        };
        if source.token != self.token {
            return;
        }
        source.cache.push(&unit);
        // Errs only when nobody is reading, which is not an error.
        let _ = source.feed.send(unit);
    }

    /// Waits until another publisher takes this path, or the path goes.
    ///
    /// A driver waits on this alongside its socket, so that a publisher
    /// which has stopped sending — the usual reason another one is
    /// reconnecting — still finds out and closes rather than holding a
    /// connection nothing is coming down.
    pub async fn evicted(&mut self) {
        // Resolves when the sender is dropped, which is what replacing the
        // source does.
        let _ = (&mut self.evicted).await;
    }

    /// The path this publishes to.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        // Only if this is still the publisher. One that was displaced must
        // not take the replacement's stream down on its way out.
        let mut source = self.path.source.lock().expect("no panic holds this");
        if source
            .as_ref()
            .is_some_and(|source| source.token == self.token)
        {
            *source = None;
            drop(source);
            self.registry
                .paths
                .write()
                .expect("no panic holds this")
                .remove(&self.name);
        }
    }
}

/// One reader's view of a path.
///
/// Dropping it detaches; the publisher is not told and does not care.
pub struct Reader {
    description: Arc<Description>,
    backlog: VecDeque<Unit>,
    feed: broadcast::Receiver<Unit>,
    skipped: u64,
}

impl Reader {
    /// What the stream is, which every protocol has to state before it sends
    /// anything.
    pub fn description(&self) -> &Description {
        &self.description
    }

    /// How many units this reader has been made to skip past for falling
    /// behind. Worth saying out loud: it means the reader's own connection
    /// is not keeping up.
    pub fn skipped(&self) -> u64 {
        self.skipped
    }

    /// The next unit, or `None` once the publisher has gone.
    ///
    /// Cancel-safe: dropping the future loses nothing, so this can sit in a
    /// `select!` beside a socket.
    pub async fn next(&mut self) -> Option<Unit> {
        if let Some(unit) = self.backlog.pop_front() {
            return Some(unit);
        }
        match self.feed.recv().await {
            Ok(unit) => Some(unit),
            Err(broadcast::error::RecvError::Closed) => None,
            Err(broadcast::error::RecvError::Lagged(units)) => {
                self.skipped += units;
                // Everything between here and the next picture the stream
                // can be started at refers to units this reader no longer
                // has. See the module docs.
                self.resync().await?;
                self.backlog.pop_front()
            }
        }
    }

    /// Reads forward to the next place a stream can be started, putting it
    /// at the front of the backlog.
    async fn resync(&mut self) -> Option<()> {
        loop {
            match self.feed.recv().await {
                Ok(unit) if unit.opens_a_stream() => {
                    self.backlog.push_back(unit);
                    return Some(());
                }
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(units)) => {
                    self.skipped += units;
                }
            }
        }
    }
}

/// What a reader that has just joined is given.
///
/// One group of pictures: everything since the last one a stream can be
/// started at. A reader handed that has something to decode at once, rather
/// than a black screen until the publisher's next keyframe — which at a two
/// second interval is a second of nothing on average.
struct Cache {
    units: VecDeque<Unit>,
    bytes: usize,
    /// Set when a group grew past what is worth keeping, and cleared at the
    /// next one. While it is set nothing is kept: half a group is not
    /// something a reader can start on, so there is no point holding it.
    overflowed: bool,
    /// Whether the stream has pictures. Without them there is nothing to cut
    /// a group at, and since every frame of sound stands alone the oldest
    /// can simply be dropped instead.
    pictures: bool,
}

impl Cache {
    fn new() -> Self {
        Self {
            units: VecDeque::new(),
            bytes: 0,
            overflowed: false,
            pictures: false,
        }
    }

    fn push(&mut self, unit: &Unit) {
        if unit.opens_a_stream() {
            self.units.clear();
            self.bytes = 0;
            self.overflowed = false;
            self.pictures = true;
        }
        if self.overflowed {
            return;
        }
        self.units.push_back(unit.clone());
        self.bytes += unit.len();
        while self.bytes > CACHE_LIMIT {
            if self.pictures {
                // A group too long to keep. Everything of it goes, because
                // what is left would not start a reader.
                self.units.clear();
                self.bytes = 0;
                self.overflowed = true;
                return;
            }
            let Some(oldest) = self.units.pop_front() else {
                return;
            };
            self.bytes -= oldest.len();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;

    use super::*;
    use crate::codec::{aac, h264};
    use crate::track::{Codec, TrackId};
    use crate::unit::{AudioPayload, AudioUnit, VideoPayload, VideoUnit};

    fn description() -> Description {
        Description::new(vec![
            Codec::H264(h264::Parameters {
                sps: vec![h264::Nal::new(Bytes::from_static(&[0x67, 0x42, 0xc0, 0x1e])).unwrap()],
                pps: vec![h264::Nal::new(Bytes::from_static(&[0x68, 0xce])).unwrap()],
            }),
            Codec::Aac(aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap()),
        ])
        .unwrap()
    }

    fn track(index: usize) -> TrackId {
        description().tracks()[index].id()
    }

    /// A picture, keyframe or not, at `pts` milliseconds.
    fn picture(pts: u64, keyframe: bool) -> Unit {
        let header = if keyframe { 0x65 } else { 0x41 };
        Unit::Video(VideoUnit::new(
            track(0),
            VideoPayload::H264(vec![
                h264::Nal::new(Bytes::from_owner(vec![header, pts as u8])).unwrap(),
            ]),
            Duration::from_millis(pts),
            Duration::from_millis(pts),
        ))
    }

    fn sound(pts: u64) -> Unit {
        Unit::Audio(AudioUnit {
            track: track(1),
            payload: AudioPayload::Aac(Bytes::from_owner(vec![0x21, pts as u8])),
            pts: Duration::from_millis(pts),
        })
    }

    /// Everything a reader has waiting for it, without waiting.
    fn drain(reader: &mut Reader) -> Vec<Unit> {
        let mut units = Vec::new();
        while let Ok(unit) = reader.feed.try_recv() {
            units.push(unit);
        }
        reader.backlog.drain(..).chain(units).collect()
    }

    #[tokio::test]
    async fn a_reader_gets_what_a_publisher_sends() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        let mut reader = registry.read("live/cam1").expect("a stream");

        publisher.push(picture(0, true));
        publisher.push(sound(10));
        assert_eq!(reader.next().await, Some(picture(0, true)));
        assert_eq!(reader.next().await, Some(sound(10)));
    }

    #[tokio::test]
    async fn a_reader_is_told_what_the_stream_is_before_anything_else() {
        let registry = Registry::new();
        let _publisher = registry.publish("live/cam1", description());
        let reader = registry.read("live/cam1").unwrap();
        assert_eq!(reader.description(), &description());
    }

    #[tokio::test]
    async fn reading_a_name_nothing_is_published_to_finds_nothing() {
        let registry = Registry::new();
        assert!(registry.read("live/cam1").is_none());
    }

    #[tokio::test]
    async fn a_reader_that_joins_late_starts_at_the_last_keyframe() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());

        publisher.push(picture(0, true));
        publisher.push(picture(33, false));
        // A second group. The first is no longer worth handing anyone.
        publisher.push(picture(66, true));
        publisher.push(sound(70));
        publisher.push(picture(99, false));

        let mut reader = registry.read("live/cam1").unwrap();
        assert_eq!(
            drain(&mut reader),
            vec![picture(66, true), sound(70), picture(99, false)]
        );
    }

    #[tokio::test]
    async fn a_reader_that_joins_before_any_keyframe_gets_nothing_to_start_on() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        publisher.push(picture(0, false));

        let mut reader = registry.read("live/cam1").unwrap();
        // Handing over pictures that refer to one nobody has would decode
        // into smears. It waits for the next keyframe instead.
        assert_eq!(drain(&mut reader), vec![picture(0, false)]);
    }

    #[tokio::test]
    async fn nothing_sent_while_a_reader_is_attaching_falls_between() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        publisher.push(picture(0, true));

        let mut reader = registry.read("live/cam1").unwrap();
        publisher.push(picture(33, false));

        // The keyframe from the cache, then the one that arrived after the
        // subscription, and neither twice.
        assert_eq!(reader.next().await, Some(picture(0, true)));
        assert_eq!(reader.next().await, Some(picture(33, false)));
    }

    #[tokio::test]
    async fn a_reader_that_falls_behind_skips_to_where_it_can_start_again() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        let mut reader = registry.read("live/cam1").unwrap();

        // Overrun the backlog without reading any of it.
        for n in 0..BACKLOG as u64 + 10 {
            publisher.push(picture(n, false));
        }
        publisher.push(picture(9000, true));
        publisher.push(picture(9001, false));

        // Not the oldest thing still in the queue: the next picture the
        // stream can be started at.
        assert_eq!(reader.next().await, Some(picture(9000, true)));
        assert_eq!(reader.next().await, Some(picture(9001, false)));
        assert!(reader.skipped() > 0);
    }

    #[tokio::test]
    async fn a_publisher_that_no_reader_is_keeping_up_with_is_not_slowed_down() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        let _reader = registry.read("live/cam1").unwrap();

        // Far past the backlog, with nothing reading. Every one of these
        // returns; none of them can fail.
        for n in 0..BACKLOG as u64 * 4 {
            publisher.push(picture(n, n % 30 == 0));
        }
    }

    #[tokio::test]
    async fn a_reader_learns_when_the_publisher_goes() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());
        let mut reader = registry.read("live/cam1").unwrap();
        publisher.push(picture(0, true));
        drop(publisher);

        assert_eq!(reader.next().await, Some(picture(0, true)));
        assert_eq!(reader.next().await, None);
        assert!(registry.read("live/cam1").is_none());
    }

    #[tokio::test]
    async fn a_second_publisher_takes_the_path_and_the_first_is_told() {
        let registry = Registry::new();
        let mut first = registry.publish("live/cam1", description());
        let mut reader = registry.read("live/cam1").unwrap();

        let second = registry.publish("live/cam1", description());
        // The wait resolves at once: the first publisher is displaced.
        first.evicted().await;

        // Its readers ended with it rather than being moved across, since
        // the new stream is described separately.
        assert_eq!(reader.next().await, None);

        // And what it pushes now goes nowhere.
        first.push(picture(0, true));
        let mut moved = registry.read("live/cam1").unwrap();
        second.push(picture(50, true));
        assert_eq!(moved.next().await, Some(picture(50, true)));
    }

    #[tokio::test]
    async fn a_displaced_publisher_does_not_take_the_path_with_it() {
        let registry = Registry::new();
        let first = registry.publish("live/cam1", description());
        let second = registry.publish("live/cam1", description());
        drop(first);

        let mut reader = registry
            .read("live/cam1")
            .expect("the second is still there");
        second.push(picture(0, true));
        assert_eq!(reader.next().await, Some(picture(0, true)));
    }

    #[tokio::test]
    async fn a_group_too_long_to_keep_is_not_kept_in_halves() {
        let registry = Registry::new();
        let publisher = registry.publish("live/cam1", description());

        publisher.push(picture(0, true));
        let huge = Unit::Video(VideoUnit::new(
            track(0),
            VideoPayload::H264(vec![
                h264::Nal::new(Bytes::from_owner(vec![0x41; CACHE_LIMIT + 1])).unwrap(),
            ]),
            Duration::ZERO,
            Duration::ZERO,
        ));
        publisher.push(huge);
        publisher.push(picture(66, false));

        // Half a group would not start a reader, so none of it is offered.
        let mut reader = registry.read("live/cam1").unwrap();
        assert!(drain(&mut reader).is_empty());

        // And the next keyframe puts it back to work.
        publisher.push(picture(99, true));
        let mut reader = registry.read("live/cam1").unwrap();
        assert_eq!(drain(&mut reader), vec![picture(99, true)]);
    }

    #[tokio::test]
    async fn a_stream_with_no_pictures_keeps_what_it_can_and_drops_the_oldest() {
        let registry = Registry::new();
        let description = Description::new(vec![Codec::Aac(
            aac::Parameters::parse(Bytes::from_static(&[0x12, 0x10])).unwrap(),
        )])
        .unwrap();
        let only = description.tracks()[0].id();
        let publisher = registry.publish("live/radio", description);

        // Nothing here opens a group, so the cache can only drop the oldest
        // — which is correct, since every frame of sound stands alone.
        let big = CACHE_LIMIT / 4 + 1;
        for n in 0..6u64 {
            publisher.push(Unit::Audio(AudioUnit {
                track: only,
                payload: AudioPayload::Aac(Bytes::from_owner(vec![n as u8; big])),
                pts: Duration::from_millis(n),
            }));
        }

        let mut reader = registry.read("live/radio").unwrap();
        let held = drain(&mut reader);
        assert!(!held.is_empty(), "sound is still worth handing over");
        assert!(held.len() < 6, "and not all of it was kept");
    }

    #[tokio::test]
    async fn the_registry_lists_what_is_being_published() {
        let registry = Registry::new();
        let first = registry.publish("live/cam1", description());
        let _second = registry.publish("live/cam2", description());

        let mut names = registry.names();
        names.sort();
        assert_eq!(names, ["live/cam1", "live/cam2"]);

        drop(first);
        assert_eq!(registry.names(), ["live/cam2"]);
    }
}
