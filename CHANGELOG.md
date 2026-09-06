# Changelog

All notable changes to this project are documented in this file.

## Unreleased

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
