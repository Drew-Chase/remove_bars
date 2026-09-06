use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

const TOOL: &str = env!("CARGO_BIN_EXE_remove_bars");

fn ffmpeg_and_ffprobe_available() -> bool {
    let ffmpeg = Command::new("ffmpeg").arg("-version").output().is_ok();
    let ffprobe = Command::new("ffprobe").arg("-version").output().is_ok();
    if !(ffmpeg && ffprobe) {
        eprintln!("skipping test: ffmpeg/ffprobe not available");
    }
    ffmpeg && ffprobe
}

fn run_tool(input: &Path, output: &Path) -> String {
    let result = Command::new(TOOL)
        .args([
            "-i",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .output()
        .expect("failed to run remove_bars");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.status.success(),
        "remove_bars exited with {result:?}\n--- log ---\n{log}"
    );
    log
}

fn write_subtitle_file(dir: &Path) -> PathBuf {
    let path = dir.join("subs.srt");
    fs::write(
        &path,
        "1\n00:00:00,000 --> 00:00:02,000\nHello bars\n\n2\n00:00:05,000 --> 00:00:07,000\nSecond line\n",
    )
    .unwrap();
    path
}

/// Creates a 35s letterboxed video (320x180 content padded into a 320x240
/// frame). With `multi_stream`, audio and subtitle streams are added, matching
/// the media files that exposed the `-filter` vs `-vf` regression.
fn create_letterboxed_video(dir: &Path, name: &str, multi_stream: bool) -> PathBuf {
    let output = dir.join(name);
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:duration=35:rate=25",
        ]);

    if multi_stream {
        let subtitles = write_subtitle_file(dir);
        command
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=35"])
            .arg("-i")
            .arg(&subtitles);
    }

    command.args(["-vf", "pad=320:240:0:30"]).args([
        "-c:v",
        "libx264",
        "-preset",
        "ultrafast",
        "-pix_fmt",
        "yuv420p",
    ]);

    if multi_stream {
        command
            .args(["-map", "0:v", "-map", "1:a", "-map", "2:s"])
            .args(["-c:a", "aac", "-c:s", "srt"]);
    }

    let status = command.arg(&output).status().expect("failed to run ffmpeg");
    assert!(status.success(), "ffmpeg failed to create test fixture");

    output
}

/// Creates a 35s video that fills its frame completely (no black bars).
fn create_full_frame_video(dir: &Path, name: &str) -> PathBuf {
    let output = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:duration=35:rate=25",
        ])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(&output)
        .status()
        .expect("failed to run ffmpeg");
    assert!(status.success(), "ffmpeg failed to create test fixture");

    output
}

fn probe_video_size(path: &Path) -> (u32, u32) {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("failed to run ffprobe");

    let text = String::from_utf8_lossy(&output.stdout);
    let mut values = text.trim().split(',');
    let width = values.next().unwrap().parse().unwrap();
    let height = values.next().unwrap().parse().unwrap();
    (width, height)
}

fn has_stream_type(path: &Path, selector: &str) -> bool {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            selector,
            "-show_entries",
            "stream=index",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("failed to run ffprobe");

    !output.stdout.is_empty()
}

#[test]
fn letterboxed_video_is_cropped() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_letterboxed_video(&input_dir, "video.mkv", false);
    let output_dir = temp.path().join("output");

    let log = run_tool(&input_dir, &output_dir);

    assert!(log.contains("Detected: crop=320:176:0:32"), "log:\n{log}");
    assert!(log.contains("SUCCESS"), "log:\n{log}");

    let output_file = output_dir.join("video.mkv");
    assert_eq!(probe_video_size(&output_file), (320, 176));
    assert!(fs::metadata(&video).unwrap().len() > 0);
}

#[test]
fn multi_stream_video_is_cropped() {
    // Regression test: files containing audio and subtitle streams in addition
    // to video made crop detection fail ("Filtergraph has a video output,
    // cannot connect it to audio output stream") when the filtergraph was
    // passed with `-filter` instead of `-vf`.
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_letterboxed_video(&input_dir, "video.mkv", true);
    assert!(has_stream_type(&video, "a"), "fixture should contain audio");
    assert!(
        has_stream_type(&video, "s"),
        "fixture should contain subtitles"
    );
    let output_dir = temp.path().join("output");

    let log = run_tool(&input_dir, &output_dir);

    assert!(log.contains("Detected: crop=320:176:0:32"), "log:\n{log}");
    assert!(log.contains("SUCCESS"), "log:\n{log}");

    let output_file = output_dir.join("video.mkv");
    assert_eq!(probe_video_size(&output_file), (320, 176));
    assert!(
        has_stream_type(&output_file, "a"),
        "audio stream should be preserved"
    );
    assert!(
        has_stream_type(&output_file, "s"),
        "subtitle stream should be preserved"
    );
}

#[test]
fn full_frame_video_is_copied_unchanged() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_full_frame_video(&input_dir, "video.mkv");
    let output_dir = temp.path().join("output");

    let log = run_tool(&input_dir, &output_dir);

    assert!(log.contains("No black bars detected"), "log:\n{log}");
    assert!(
        log.contains("SUCCESS: Copied without changes"),
        "log:\n{log}"
    );

    let output_file = output_dir.join("video.mkv");
    assert_eq!(
        fs::read(&video).unwrap(),
        fs::read(&output_file).unwrap(),
        "output should be a byte-identical copy of the input"
    );
}

#[test]
fn existing_output_files_are_skipped() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    let output_dir = temp.path().join("output");
    fs::create_dir(&input_dir).unwrap();
    fs::create_dir(&output_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);

    let output_file = output_dir.join("video.mkv");
    fs::write(&output_file, "pre-existing").unwrap();

    let log = run_tool(&input_dir, &output_dir);

    assert!(
        log.contains("SKIPPED: Output file already exists"),
        "log:\n{log}"
    );
    assert_eq!(fs::read(&output_file).unwrap(), b"pre-existing");
}
