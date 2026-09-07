# Changelog

All notable changes to this project are documented in this file.

## 1.0.1 - 2026-09-07

### Added

- Resumable cropping: when cropping from a scan database, each successfully
  cropped file is recorded in the database. Re-running the same command skips
  files that were already cropped and continues where the previous run left
  off, so long jobs can be stopped and resumed. Failed files are retried on
  the next run, and scan databases created by older versions are upgraded in
  place.

## 1.0.0 - 2026-09-06

### Added

- `scan` subcommand: analyzes every video under a directory with cropdetect
  and indexes the results (relative path, detected crop value, source
  resolution, whether cropping is needed) into an SQLite database, without
  modifying any files. Detection runs in parallel using rayon
  (`-j` / `--parallel`, defaulting to the CPU core count); results are
  buffered in memory and written to the database in batches of 100, with a
  progress bar showing overall progress. Re-running a scan replaces the
  previous database contents.
- `crop` subcommand: replaces the previous top-level command and accepts
  either a directory or a scan database as input. Database input reuses the
  indexed crop values instead of re-running detection, reports indexed files
  that have since been moved or deleted, and supports all existing options
  (encoder selection, in-place mode, copy-uncropped).
- `--overwrite-original`: encodes to a temporary file next to the original and
  replaces the original only after a successful encode, for in-place
  automation. `-o` / `--output` is ignored in this mode. Uncropped files are
  left untouched, so re-running on already-cropped libraries is safe.
- `--copy-uncropped`: copies files that need no cropping to the output
  directory.
- `--threshold` option for `scan` and `crop` (default 8 pixels): detected
  crops that trim at most this many pixels from any edge of the frame are
  treated as needing no crop, so round-to-16 detection artifacts no longer
  trigger pointless re-encodes. Cropping from a scan database re-applies the
  threshold against the indexed crop values, so it can be more aggressive
  than the scan was.
- `--crop-detect-start` option for `scan` and `crop`: chooses where crop
  detection starts in each video (default 30s), used together with
  `--crop-detect-seconds` to control the analyzed window.
- Redesigned two-line progress bars for scanning and encoding: the label
  (including file name, speed and fps for encodes) sits on the left and the
  percentage, frame/file count and ETA are right-aligned on the same line,
  with the bar on its own full-width line below. Long file names are
  truncated with an ellipsis, and the ETA is estimated from elapsed time and
  progress rate.

### Changed

- `remove_bars` now requires a subcommand (`scan` or `crop`); running it
  without one prints help.
- Files that need no cropping are now skipped by default instead of being
  copied to the output directory; pass `--copy-uncropped` to restore the
  previous behavior.

## 0.2.0 - 2026-09-06

### Added

- Hardware-accelerated encoding via `-e` / `--encoder`: `nvenc` (NVIDIA),
  `amf` (AMD) and `qsv` (Intel), alongside the default software `x264`
  encoder. The quality factor and preset are translated to each encoder's own
  options (CRF/CQ/global quality/QP, x264 presets to NVENC `p1`–`p7` or AMF
  `speed`/`balanced`/`quality`), and the selected encoder is verified against
  the installed ffmpeg build before processing starts.

## 0.1.1 - 2026-09-06

### Fixed

- Crop detection and encoding now work on files that contain audio and
  subtitle streams in addition to video (e.g. TV episodes with dozens of
  subtitle tracks). The ffmpeg filtergraph is passed with `-vf`, which binds it
  to video streams only; the previous invocation failed on such files with
  `Filtergraph has a video output, cannot connect it to audio output stream`.
- Failed files now show ffmpeg's actual error output instead of a generic
  failure message.

### Added

- Progress bar during encoding, showing the current frame against the
  estimated total number of frames (input duration x framerate), with speed,
  fps and ETA. Falls back to a spinner when the total cannot be estimated, and
  is hidden when output is piped.
- `REMOVE_BARS_DEBUG=1` environment variable to print the exact ffmpeg
  commands being run.
- Test suite: unit tests for crop parsing/selection and end-to-end
  integration tests against real ffmpeg-encoded fixtures.

## 0.1.0 - 2026-09-06

### Added

- Initial release.
- Batch black-bar removal: ffmpeg's `cropdetect` filter samples a window of
  each video (starting 30s in, configurable duration), the most frequently
  reported crop value is applied, and the video is re-encoded with libx264
  (configurable CRF and preset) while audio and subtitle streams are
  stream-copied.
- Files where the detected crop equals the full frame are copied unchanged.
- The input directory tree is mirrored into the output directory; already
  processed files are skipped, so interrupted runs can be resumed.
- Cross-platform release automation via GitHub Actions: tagged versions build
  Windows (x64), macOS (Apple Silicon and Intel) and Linux (x64, static)
  binaries and publish them as archives on the GitHub release.
