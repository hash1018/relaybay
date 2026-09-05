//! Publishes with ffmpeg over RTMP, reads back with ffmpeg over RTSP, and
//! checks that what came out is what went in.
//!
//! Not a unit test: it needs ffmpeg on the path and two ports to bind. What
//! it checks is the thing no test written against this crate can — that two
//! ends which have never seen this code agree about what crossed it.
//!
//! ```text
//! ffmpeg ──RTMP──▶ relaybay ──RTSP──▶ ffmpeg ──▶ a file to count
//! ```

use std::process::{Command, Stdio};
use std::time::Duration;

use relaybay::server::{Config, Server};

const RTMP: u16 = 11935;
const RTSP: u16 = 18554;

/// Whichever H.264 encoder this ffmpeg was built with.
fn video_encoder() -> &'static str {
    let built = Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .expect("ffmpeg on the path");
    let built = String::from_utf8_lossy(&built.stdout);
    ["libx264", "libopenh264", "h264_mf", "h264_nvenc"]
        .into_iter()
        .find(|candidate| built.contains(candidate))
        .expect("this ffmpeg has no H.264 encoder")
}

fn main() {
    let server = Server::start(Config {
        rtmp: Some(([127, 0, 0, 1], RTMP).into()),
        rtsp: Some(([127, 0, 0, 1], RTSP).into()),
        worker_threads: 2,
    })
    .expect("bound");

    let mut publisher = Command::new("ffmpeg")
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
            "8",
            "-f",
            "flv",
            &format!("rtmp://127.0.0.1:{RTMP}/live/cam1"),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("ffmpeg starts");

    // Wait for the path, so the player does not race the publisher.
    for _ in 0..200 {
        if server.registry().describe("live/cam1").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let description = server
        .registry()
        .describe("live/cam1")
        .expect("ffmpeg never published");
    println!("\n=== what relaybay was told ===");
    for track in description.tracks() {
        println!(
            "  track {}: {:?} at {} Hz",
            track.id().index(),
            track.kind(),
            track.codec().clock_rate()
        );
    }

    let out = std::env::temp_dir().join("relaybay-relayed.mkv");
    let _ = std::fs::remove_file(&out);
    let player = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-rtsp_transport",
            "tcp",
            "-i",
            &format!("rtsp://127.0.0.1:{RTSP}/live/cam1"),
            "-t",
            "5",
            "-c",
            "copy",
            out.to_str().unwrap(),
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("ffmpeg starts");

    let _ = publisher.wait();
    server.shutdown();
    assert!(player.success(), "the player did not finish cleanly");

    // What ffmpeg made of what it received, read back from the file it
    // wrote. Nothing in this crate produced these numbers.
    let probe = Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "stream=codec_name,width,height,sample_rate,channels",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("ffprobe on the path");
    let report = String::from_utf8_lossy(&probe.stdout).replace('\r', "");

    println!("\n=== what came out the other side ===");
    for line in report.lines() {
        println!("  {line}");
    }
    println!("\n  file: {}", out.display());

    assert!(report.contains("codec_name=h264"), "{report}");
    assert!(report.contains("width=320"), "{report}");
    assert!(report.contains("height=240"), "{report}");
    assert!(report.contains("codec_name=aac"), "{report}");
    assert!(report.contains("sample_rate=44100"), "{report}");
    let duration: f64 = report
        .lines()
        .find_map(|line| line.strip_prefix("duration="))
        .and_then(|value| value.parse().ok())
        .expect("a duration");
    assert!(duration > 3.0, "only {duration} seconds came through");

    println!("\nOK — {duration:.1}s of 320x240 H.264 and 44.1 kHz AAC crossed the relay");
}
