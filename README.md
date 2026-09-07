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

The input directory structure is mirrored into the output directory, and files
that already exist in the output directory are skipped, so the tool is safe to
re-run. Files that need no cropping are skipped as well unless
`--copy-uncropped` is given. Original files are only modified with
`--overwrite-original`.

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

`remove_bars` has two subcommands: `scan` (index which videos need cropping
into an SQLite database) and `crop` (crop videos in a directory, or videos
indexed by a previous scan).

### Scan

Analyze every video under a directory and index the result into an SQLite
database, without modifying any files:

```sh
remove_bars scan -i ./videos -o scan.sqlite -j 8
```

Detection runs in parallel (`-j` / `--parallel`, defaulting to the number of
CPU cores); results are buffered in memory and written to the database in
batches. The database records, for each video, its path relative to the
scanned directory, the detected `crop=W:H:X:Y` value, the source resolution
and whether it needs cropping. Re-running a scan replaces the previous
database contents.

A `--threshold` (in pixels, default 8) ignores crops that only trim a few
pixels from the frame edges — typically round-to-16 artifacts — so thin
detection noise does not trigger pointless re-encodes.

### Crop

```sh
remove_bars crop -i ./videos -o ./cropped
remove_bars crop -i scan.sqlite -o ./cropped --overwrite-original -e nvenc
```

`-i` accepts either a directory (crops are detected on the fly) or a scan
database (the indexed crop values are reused, so detection is skipped). Files
that were indexed but have since been moved or deleted are reported and
skipped.

### Arguments

| Option | Value | Default | Description | Available in |
|--------|-------|---------|-------------|--------------|
| `-i`, `--input` | `<DIR>` / `<FILE>` | `input` | Directory of videos (or scan database for `crop`) | `scan`, `crop` |
| `-o`, `--output` | `<FILE>` / `<DIR>` | `scan.sqlite` / `output` | SQLite database file (`scan`) or output directory (`crop`) | `scan`, `crop` |
| `-j`, `--parallel` | `<N>` | CPU count | Number of videos to scan in parallel | `scan` |
| `-s`, `--crop-detect-seconds` | `<SECONDS>` | `60` | Seconds of video to analyze, starting at `--crop-detect-start` | `scan`, `crop` |
| `--crop-detect-start` | `<SECONDS>` | `30` | Seconds into the video where crop detection starts | `scan`, `crop` |
| `--threshold` | `<PIXELS>` | `8` | Ignore detected crops that trim at most this many pixels from any edge | `scan`, `crop` |
| `-c`, `--crf` | `<CRF>` | `18` | Quality factor for the H.264 encode, 0–51 (lower = higher quality); translated per encoder, see [hardware acceleration](#hardware-acceleration) | `crop` |
| `-e`, `--encoder` | `<ENCODER>` | `x264` | Video encoder: `x264` (CPU), `nvenc` (NVIDIA), `amf` (AMD) or `qsv` (Intel) | `crop` |
| `-p`, `--preset` | `<PRESET>` | `medium` | x264-style encoding preset (`ultrafast` … `veryslow`); hardware encoders map it to their own presets | `crop` |
| `--copy-uncropped` | | off | Copy files that need no cropping to the output directory | `crop` |
| `--overwrite-original` | | off | Replace original files in-place after a successful encode | `crop` |
| `-h`, `--help` | | | Print help | |

### Hardware acceleration

Encoding can be offloaded to the GPU with `-e` / `--encoder`:

| Value | ffmpeg encoder | Hardware |
|-------|----------------|----------|
| `x264` (default) | libx264 | CPU |
| `nvenc` | h264_nvenc | NVIDIA (NVENC) |
| `amf` | h264_amf | AMD (AMF / VCE) |
| `qsv` | h264_qsv | Intel (Quick Sync Video) |

```sh
remove_bars crop -i ./videos -o ./cropped -e nvenc
```

The quality factor (`-c`) is translated for each encoder: CRF for x264, CQ
with VBR rate control for NVENC, global quality for QSV and constant QP for
AMF. The preset (`-p`) is passed to x264 and QSV unchanged, mapped to NVENC's
`p1`–`p7` presets (e.g. `medium` → `p4`, `veryslow` → `p7`), and mapped to
AMF's `speed`/`balanced`/`quality` presets. NVENC preset names (`p1`–`p7`)
are passed through as-is.

If the selected encoder is not compiled into the installed ffmpeg build, the
tool exits before processing any files. A missing GPU or driver surfaces as an
ffmpeg error when the first file is encoded. The `--threshold` option also
applies when cropping from a scan database, so a higher value than the scan's
can still skip files.

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
