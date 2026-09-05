//! Publishes with ffmpeg and reads what comes out the other side.
//!
//! Not a unit test: it needs ffmpeg on the path and a port to bind. What it
//! checks is the thing no test written against this crate's own encoder can
//! — that a real one is understood.

use std::process::{Command, Stdio};
use std::time::Duration;

use relaybay::server::{Config, Server};
use relaybay::track::Kind;
use relaybay::unit::Unit;

/// Whichever H.264 encoder this ffmpeg was built with. The stream is the
/// same either way; only the build differs.
fn video_encoder() -> &'static str {
    let built = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .expect("ffmpeg on the path");
    let built = String::from_utf8_lossy(&built.stdout);
    for candidate in ["libx264", "libopenh264", "h264_mf", "h264_nvenc"] {
        if built.contains(candidate) {
            return candidate;
        }
    }
    panic!("this ffmpeg has no H.264 encoder");
}

fn main() {
    let server = Server::start(Config {
        rtmp: Some(([127, 0, 0, 1], 11935).into()),
        rtsp: Some(([127, 0, 0, 1], 18554).into()),
        worker_threads: 2,
    })
    .expect("bound");
    let registry = server.registry().clone();

    let mut ffmpeg = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=30",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-c:v",
            video_encoder(),
            "-b:v",
            "800k",
            "-g",
            "30",
            "-c:a",
            "aac",
            "-b:a",
            "64k",
            "-t",
            "4",
            "-f",
            "flv",
            "rtmp://127.0.0.1:11935/live/cam1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("ffmpeg starts");

    // Wait for the path to turn up.
    let mut reader = None;
    for _ in 0..200 {
        if let Some(found) = registry.read("live/cam1") {
            reader = Some(found);
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let mut reader = reader.expect("ffmpeg never published");

    let description = reader.description().clone();
    println!("\n=== description ===");
    for track in description.tracks() {
        println!(
            "  track {}: {:?} {:?} at {} Hz",
            track.id().index(),
            track.kind(),
            track.codec(),
            track.codec().clock_rate()
        );
    }

    // Read for a couple of seconds on a runtime of our own.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let (pictures, keyframes, sounds, last) = runtime.block_on(async {
        let mut pictures = 0u32;
        let mut keyframes = 0u32;
        let mut sounds = 0u32;
        let mut last = Duration::ZERO;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(6);
        loop {
            let Ok(Some(unit)) = tokio::time::timeout_at(deadline, reader.next()).await else {
                break;
            };
            last = unit.pts();
            match unit {
                Unit::Video(unit) => {
                    pictures += 1;
                    keyframes += u32::from(unit.keyframe);
                }
                Unit::Audio(_) => sounds += 1,
            }
        }
        (pictures, keyframes, sounds, last)
    });

    let _ = ffmpeg.wait();
    server.shutdown();

    println!("\n=== read ===");
    println!("  pictures  {pictures} ({keyframes} keyframes)");
    println!("  sounds    {sounds}");
    println!("  last pts  {last:?}");
    println!("  skipped   {}", reader.skipped());

    assert_eq!(description.tracks().len(), 2, "video and audio");
    assert_eq!(description.of_kind(Kind::Video).count(), 1);
    assert_eq!(description.of_kind(Kind::Audio).count(), 1);
    assert!(pictures > 60, "four seconds at 30 fps, got {pictures}");
    assert!(keyframes >= 3, "one every 30 pictures, got {keyframes}");
    assert!(sounds > 100, "four seconds of AAC, got {sounds}");
    assert!(last > Duration::from_secs(3), "a timeline that advances");
    println!("\nOK");
}
