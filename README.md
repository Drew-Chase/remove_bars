# remove_bars

[![Latest Release](https://img.shields.io/github/v/release/Drew-Chase/remove_bars)](https://github.com/Drew-Chase/remove_bars/releases/latest)

Batch-detect and remove black bars (letterboxing / pillarboxing) from videos using
[ffmpeg](https://ffmpeg.org)'s `cropdetect` filter, then re-encode them with the bars cropped away.

## How it works

For every video found under the input directory:

1. `cropdetect` (noise limit 24, dimension rounding 16) analyzes a sample window of the video, starting 30 seconds in
2. The most frequently reported `crop=W:H:X:Y` value is selected
3. If the detected crop equals the full frame (no bars present), the file is copied unchanged
4. Otherwise the video is re-encoded with the crop applied (`libx264`, configurable CRF and preset, audio and subtitle streams stream-copied)

The input directory structure is mirrored into the output directory. Original files are never modified, and files that already exist in the output directory are skipped, so the tool is safe to re-run.

## Requirements

- [ffmpeg](https://ffmpeg.org) available in `PATH` (the program exits with an error if it is missing)

## Installation

### From the releases page

Download the archive for your platform from the [latest release](https://github.com/Drew-Chase/remove_bars/releases/latest), extract it, and place the binary somewhere in your `PATH`.

| Platform | Archive |
|----------|---------|
| Windows (x64) | `remove_bars-<version>-x86_64-pc-windows-msvc.zip` |
| macOS (Apple Silicon) | `remove_bars-<version>-aarch64-apple-darwin.tar.gz` |
| macOS (Intel) | `remove_bars-<version>-x86_64-apple-darwin.tar.gz` |
| Linux (x64, static) | `remove_bars-<version>-x86_64-unknown-linux-musl.tar.gz` |

### With Cargo

Requires a Rust toolchain (<https://rustup.rs>).

```sh
cargo install --git https://github.com/Drew-Chase/remove_bars
```

This installs the `remove_bars` binary into `~/.cargo/bin`.

## Usage

```sh
remove_bars --input ./videos --output ./cropped --crf 20 --preset slow
```

### Arguments

| Option | Value | Default | Description |
|--------|-------|---------|-------------|
| `-i`, `--input` | `<DIR>` | `input` | Directory containing the videos to process |
| `-o`, `--output` | `<DIR>` | `output` | Directory to write processed videos to |
| `-s`, `--crop-detect-seconds` | `<SECONDS>` | `60` | Seconds of video to analyze, starting 30s into the file |
| `-c`, `--crf` | `<CRF>` | `18` | Constant rate factor for the H.264 encode, 0–51 (lower = higher quality) |
| `-p`, `--preset` | `<PRESET>` | `medium` | x264 encoding preset (`ultrafast` … `veryslow`) |
| `-h`, `--help` | | | Print help |
| `-V`, `--version` | | | Print version |

### Example output

```text
========================================
Black Bar Removal
========================================
Input:  /media/input
Output: /media/output
Found 2 video file(s) to process
========================================

----------------------------------------
[1/2] clean.mkv
Detecting black bars...
No black bars detected
Copying file without modification...
SUCCESS: Copied without changes

----------------------------------------
[2/2] movie.mp4
Created directory: /media/output/nested
Detecting black bars...
Detected: crop=320:176:0:32
Encoding with crop filter...
SUCCESS: nested/movie.mp4

========================================
PROCESSING COMPLETE
========================================
Processed: 2
Skipped:   0
Failed:    0
Total:     2
```

## Notes

- Videos shorter than 30 seconds are copied unchanged, since crop detection starts 30s into the file
- Crop dimensions are rounded to multiples of 16 pixels (ffmpeg `cropdetect round=16`)
- Building from source: `cargo build --release`
