use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
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
    run_tool_with(input, output, &[])
}

fn run_tool_with(input: &Path, output: &Path, extra_args: &[&str]) -> String {
    let result = Command::new(TOOL)
        .args([
            "crop",
            "-i",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .args(extra_args)
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

fn run_scan(input: &Path, output: &Path) -> String {
    run_scan_with(input, output, &[])
}

fn run_scan_with(input: &Path, output: &Path, extra_args: &[&str]) -> String {
    let result = Command::new(TOOL)
        .args([
            "scan",
            "-i",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .args(extra_args)
        .output()
        .expect("failed to run remove_bars scan");

    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        result.status.success(),
        "remove_bars scan exited with {result:?}\n--- log ---\n{log}"
    );
    log
}

fn count_database_rows(db: &Path, needs_crop: bool) -> usize {
    let connection = Connection::open(db).unwrap();
    connection
        .query_row(
            "SELECT COUNT(*) FROM videos WHERE needs_crop = ?1",
            [needs_crop as i64],
            |row| row.get::<_, i64>(0),
        )
        .unwrap() as usize
}

fn mark_cropped(db: &Path, path_suffix: &str) {
    let connection = Connection::open(db).unwrap();
    connection
        .execute(
            "UPDATE videos SET cropped_at = 1 WHERE path LIKE '%' || ?1",
            [path_suffix],
        )
        .unwrap();
}

fn is_marked_cropped(db: &Path, path_suffix: &str) -> bool {
    let connection = Connection::open(db).unwrap();
    connection
        .query_row(
            "SELECT cropped_at FROM videos WHERE path LIKE '%' || ?1",
            [path_suffix],
            |row| row.get::<_, Option<i64>>(0),
        )
        .unwrap()
        .is_some()
}

fn read_crop_for(db: &Path, path_suffix: &str) -> (Option<String>, bool, String) {
    let connection = Connection::open(db).unwrap();
    connection
        .query_row(
            "SELECT crop, needs_crop, status FROM videos WHERE path LIKE '%' || ?1",
            [path_suffix],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, i64>(1)? != 0,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .unwrap()
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

/// Creates a 75s video that is full-frame for the first 40 seconds and
/// letterboxed (320x180 content padded into 320x240) for the last 35 seconds.
fn create_mixed_video(dir: &Path, name: &str) -> PathBuf {
    let output = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:duration=40:rate=25",
        ])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:duration=35:rate=25",
        ])
        .args([
            "-filter_complex",
            "[1:v]pad=320:240:0:30[b];[0:v][b]concat=n=2:v=1:a=0[out]",
        ])
        .args(["-map", "[out]"])
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
fn uncropped_files_are_skipped_by_default() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_full_frame_video(&input_dir, "video.mkv");
    let output_dir = temp.path().join("output");

    let log = run_tool(&input_dir, &output_dir);

    assert!(
        log.contains("No black bars detected - skipping"),
        "log:\n{log}"
    );
    assert!(
        !output_dir.join("video.mkv").exists(),
        "uncropped file should not be copied by default"
    );
}

#[test]
fn copy_uncropped_restores_copying() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_full_frame_video(&input_dir, "video.mkv");
    let output_dir = temp.path().join("output");

    let log = run_tool_with(&input_dir, &output_dir, &["--copy-uncropped"]);

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
fn in_place_replaces_original_after_successful_crop() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_letterboxed_video(&input_dir, "video.mkv", false);
    let original_bytes = fs::read(&video).unwrap();
    let output_dir = temp.path().join("output");

    let log = run_tool_with(&input_dir, &output_dir, &["--overwrite-original"]);

    assert!(log.contains("original replaced"), "log:\n{log}");
    assert_eq!(
        probe_video_size(&video),
        (320, 176),
        "the original file should now contain the cropped video"
    );
    assert_ne!(
        fs::read(&video).unwrap(),
        original_bytes,
        "the original file should have been replaced"
    );

    let leftovers: Vec<PathBuf> = fs::read_dir(&input_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name.to_string_lossy().contains("remove-bars"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files left behind: {leftovers:?}"
    );
    assert!(
        !output_dir.exists(),
        "output directory should not be created in in-place mode"
    );
}

#[test]
fn in_place_leaves_uncropped_files_untouched() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_full_frame_video(&input_dir, "video.mkv");
    let original_bytes = fs::read(&video).unwrap();
    let output_dir = temp.path().join("output");

    let log = run_tool_with(&input_dir, &output_dir, &["--overwrite-original"]);

    assert!(log.contains("nothing to do"), "log:\n{log}");
    assert_eq!(
        fs::read(&video).unwrap(),
        original_bytes,
        "uncropped file should be left untouched"
    );
}

#[test]
fn copy_uncropped_conflicts_with_overwrite_original() {
    let result = Command::new(TOOL)
        .args([
            "crop",
            "-i",
            "input",
            "-o",
            "output",
            "--copy-uncropped",
            "--overwrite-original",
        ])
        .output()
        .expect("failed to run remove_bars");

    assert!(
        !result.status.success(),
        "conflicting flags should be rejected"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("cannot be used with"),
        "stderr should mention the conflict:\n{stderr}"
    );
}

#[test]
fn scan_indexes_crop_values_into_sqlite() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "letterboxed.mkv", false);
    create_full_frame_video(&input_dir, "full.mkv");
    let db = temp.path().join("scan.sqlite");

    let log = run_scan(&input_dir, &db);

    assert!(
        log.contains("Needs cropping: crop=320:176:0:32"),
        "log:\n{log}"
    );
    assert!(db.exists(), "scan should create the database file");

    let (crop, needs_crop, status) = read_crop_for(&db, "letterboxed.mkv");
    assert_eq!(crop.as_deref(), Some("320:176:0:32"));
    assert!(needs_crop);
    assert_eq!(status, "scanned");

    let (crop, needs_crop, status) = read_crop_for(&db, "full.mkv");
    assert_eq!(crop.as_deref(), Some("320:240:0:0"));
    assert!(!needs_crop);
    assert_eq!(status, "scanned");

    assert_eq!(count_database_rows(&db, true), 1);
    assert_eq!(count_database_rows(&db, false), 1);
}

#[test]
fn scan_database_records_input_root() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let db = temp.path().join("nested").join("scan.sqlite");

    run_scan(&input_dir, &db);

    assert!(db.exists(), "scan should create parent directories");
    let connection = Connection::open(&db).unwrap();
    let root: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'input_root'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let expected = fs::canonicalize(&input_dir).unwrap();
    assert_eq!(Path::new(&root), expected);
}

#[test]
fn crop_from_database_uses_indexed_crop_values() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let db = temp.path().join("scan.sqlite");
    run_scan(&input_dir, &db);
    let output_dir = temp.path().join("output");

    let log = run_tool(&db, &output_dir);

    assert!(
        log.contains("Using scan result: crop=320:176:0:32"),
        "log:\n{log}"
    );
    assert!(
        !log.contains("Detecting black bars"),
        "crop should not re-detect indexed files:\n{log}"
    );
    assert!(log.contains("SUCCESS"), "log:\n{log}");

    let output_file = output_dir.join("video.mkv");
    assert_eq!(probe_video_size(&output_file), (320, 176));
}

#[test]
fn crop_from_database_skips_uncropped_files() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    let video = create_full_frame_video(&input_dir, "video.mkv");
    let db = temp.path().join("scan.sqlite");
    run_scan(&input_dir, &db);
    let output_dir = temp.path().join("output");

    let log = run_tool(&db, &output_dir);

    assert!(
        log.contains("No black bars detected - skipping"),
        "log:\n{log}"
    );
    assert!(!output_dir.join("video.mkv").exists());

    let log = run_tool_with(&db, &output_dir, &["--copy-uncropped"]);
    assert!(
        log.contains("SUCCESS: Copied without changes"),
        "log:\n{log}"
    );
    assert_eq!(
        fs::read(&video).unwrap(),
        fs::read(output_dir.join("video.mkv")).unwrap()
    );
}

#[test]
fn crop_reports_missing_files_from_database() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let db = temp.path().join("scan.sqlite");
    run_scan(&input_dir, &db);
    fs::remove_file(input_dir.join("video.mkv")).unwrap();
    let output_dir = temp.path().join("output");

    let log = run_tool(&db, &output_dir);

    assert!(log.contains("file not found"), "log:\n{log}");
    assert!(
        log.contains("Skipped:   1"),
        "missing files should count as skipped:\n{log}"
    );
}

#[test]
fn crop_fails_cleanly_on_missing_database() {
    let result = Command::new(TOOL)
        .args(["crop", "-i", "missing.sqlite", "-o", "output"])
        .output()
        .expect("failed to run remove_bars");

    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("Input path does not exist"),
        "stderr:\n{stderr}"
    );
}

#[test]
fn crop_records_progress_in_the_scan_database() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let db = temp.path().join("scan.sqlite");
    run_scan(&input_dir, &db);
    assert!(!is_marked_cropped(&db, "video.mkv"));
    let output_dir = temp.path().join("output");

    run_tool(&db, &output_dir);

    assert!(
        is_marked_cropped(&db, "video.mkv"),
        "successful crops should be recorded in the database"
    );
}

#[test]
fn crop_from_database_resumes_where_it_left_off() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "a-first.mkv", false);
    create_letterboxed_video(&input_dir, "b-second.mkv", false);
    let db = temp.path().join("scan.sqlite");
    run_scan(&input_dir, &db);

    // Simulate a previous run that finished the first file before stopping.
    mark_cropped(&db, "a-first.mkv");
    let output_dir = temp.path().join("output");

    let log = run_tool(&db, &output_dir);

    assert!(
        log.contains("Resuming: 1 file(s) already cropped and will be skipped"),
        "log:\n{log}"
    );
    assert!(
        log.contains("SUCCESS: b-second.mkv"),
        "the unprocessed file should be cropped:\n{log}"
    );
    assert!(
        !output_dir.join("a-first.mkv").exists(),
        "already-cropped files must not be re-encoded"
    );
    assert_eq!(
        probe_video_size(&output_dir.join("b-second.mkv")),
        (320, 176)
    );

    assert!(is_marked_cropped(&db, "b-second.mkv"));

    // A further run has nothing left to do and exits before the summary.
    let log = run_tool(&db, &output_dir);
    assert!(
        log.contains("All indexed file(s) have already been cropped"),
        "log:\n{log}"
    );
    assert!(
        !log.contains("PROCESSING COMPLETE"),
        "nothing to do should exit before the summary:\n{log}"
    );
}

#[test]
fn crop_upgrades_databases_without_progress_tracking() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let db = temp.path().join("scan.sqlite");

    // Build a database with the pre-1.0 schema (no cropped_at column).
    let root = fs::canonicalize(&input_dir).unwrap();
    let connection = Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE videos (
                 path       TEXT PRIMARY KEY,
                 crop       TEXT,
                 width      INTEGER,
                 height     INTEGER,
                 needs_crop INTEGER NOT NULL,
                 status     TEXT NOT NULL,
                 scanned_at INTEGER NOT NULL
             );
             INSERT INTO meta (key, value) VALUES ('input_root', 'placeholder');
             INSERT INTO videos (path, crop, width, height, needs_crop, status, scanned_at)
             VALUES ('video.mkv', '320:176:0:32', 320, 240, 1, 'scanned', 0);",
        )
        .unwrap();
    connection
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'input_root'",
            [root.to_string_lossy().to_string()],
        )
        .unwrap();
    drop(connection);

    let output_dir = temp.path().join("output");
    let log = run_tool(&db, &output_dir);

    assert!(log.contains("SUCCESS"), "log:\n{log}");
    assert_eq!(probe_video_size(&output_dir.join("video.mkv")), (320, 176));
    assert!(
        is_marked_cropped(&db, "video.mkv"),
        "the upgraded database should track progress"
    );
}

#[test]
fn crop_detect_start_controls_detection_window() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_mixed_video(&input_dir, "mixed.mkv");

    // A window inside the letterboxed second half (starts at 40s) needs
    // cropping.
    let db = temp.path().join("late.sqlite");
    run_scan_with(
        &input_dir,
        &db,
        &["--crop-detect-start", "45", "--crop-detect-seconds", "10"],
    );
    let (crop, needs_crop, _) = read_crop_for(&db, "mixed.mkv");
    assert_eq!(crop.as_deref(), Some("320:176:0:32"));
    assert!(needs_crop);

    // A window inside the full-frame first half does not.
    let db = temp.path().join("early.sqlite");
    run_scan_with(
        &input_dir,
        &db,
        &["--crop-detect-start", "5", "--crop-detect-seconds", "10"],
    );
    let (crop, needs_crop, _) = read_crop_for(&db, "mixed.mkv");
    assert_eq!(crop.as_deref(), Some("320:240:0:0"));
    assert!(!needs_crop);
}

#[test]
fn threshold_ignores_negligible_bars() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    // crop=320:176:0:32 trims 32px from the top and bottom edges.
    create_letterboxed_video(&input_dir, "video.mkv", false);
    let output_dir = temp.path().join("output");

    // Default threshold (8px): the 32px bars are real and get cropped.
    let db = temp.path().join("default.sqlite");
    let log = run_scan(&input_dir, &db);
    assert!(
        log.contains("Needs cropping: crop=320:176:0:32"),
        "log:\n{log}"
    );
    assert_eq!(count_database_rows(&db, true), 1);
    assert_eq!(count_database_rows(&db, false), 0);

    // A threshold above the bar size treats the file as needing no crop.
    let db = temp.path().join("thresholded.sqlite");
    let log = run_scan_with(&input_dir, &db, &["--threshold", "100"]);
    assert!(log.contains("No black bars detected"), "log:\n{log}");
    assert_eq!(count_database_rows(&db, true), 0);
    assert_eq!(count_database_rows(&db, false), 1);

    // Cropping from a scan database re-applies the threshold, so a higher
    // value than the scan's can still skip files.
    let db = temp.path().join("default.sqlite");
    let log = run_tool_with(&db, &output_dir, &["--threshold", "100"]);
    assert!(
        log.contains("No black bars detected - skipping"),
        "log:\n{log}"
    );
    assert!(!output_dir.join("video.mkv").exists());
}

#[test]
fn parallel_scan_produces_the_same_index() {
    if !ffmpeg_and_ffprobe_available() {
        return;
    }

    let temp = TempDir::new().unwrap();
    let input_dir = temp.path().join("input");
    fs::create_dir(&input_dir).unwrap();
    create_letterboxed_video(&input_dir, "a-letterboxed.mkv", false);
    create_full_frame_video(&input_dir, "b-full.mkv");
    create_mixed_video(&input_dir, "c-mixed.mkv");

    let serial_db = temp.path().join("serial.sqlite");
    run_scan(&input_dir, &serial_db);

    let parallel_db = temp.path().join("parallel.sqlite");
    run_scan_with(&input_dir, &parallel_db, &["-j", "4"]);

    let connection = Connection::open(&parallel_db).unwrap();
    let mut statement = connection
        .prepare("SELECT path, crop, needs_crop FROM videos ORDER BY path")
        .unwrap();
    let parallel_rows: Vec<(String, Option<String>, bool)> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    let connection = Connection::open(&serial_db).unwrap();
    let mut statement = connection
        .prepare("SELECT path, crop, needs_crop FROM videos ORDER BY path")
        .unwrap();
    let serial_rows: Vec<(String, Option<String>, bool)> = statement
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get::<_, i64>(2)? != 0))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();

    assert_eq!(parallel_rows.len(), 3);
    assert_eq!(parallel_rows, serial_rows);
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
