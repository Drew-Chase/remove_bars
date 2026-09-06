use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Path, PathBuf},
    sync::LazyLock,
};

use clap::Parser;
use color_eyre::eyre::{WrapErr, bail, eyre};
use colored::Colorize;
use ffmpeg_sidecar::{
    command::FfmpegCommand,
    event::{FfmpegEvent, LogLevel},
};
use regex::Regex;
use walkdir::WalkDir;

/// Detect and remove black bars (letterboxing/pillarboxing) from videos under
/// an input directory, mirroring the directory structure into the output
/// directory.
#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Directory containing the videos to process
    #[arg(short, long, default_value = "input")]
    input: PathBuf,

    /// Directory to write processed videos to
    #[arg(short, long, default_value = "output")]
    output: PathBuf,

    /// Seconds of video to analyze, starting 30s into the file
    #[arg(short = 's', long, default_value_t = 60)]
    crop_detect_seconds: u64,

    /// Constant rate factor for the H.264 encode (lower = higher quality)
    #[arg(short, long, default_value_t = 18)]
    crf: u32,

    /// x264 encoding preset (ultrafast ... veryslow)
    #[arg(short, long, default_value = "medium")]
    preset: String,
}

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "mov", "wmv", "m4v", "webm"];

const CROP_DETECT_START_SECS: u64 = 30;

const CROP_DETECT_FILTER: &str = "cropdetect=24:16:0";

enum Outcome {
    Processed,
    Skipped,
    Failed,
}

struct CropDetection {
    crop: Option<String>,
    source_size: Option<(u32, u32)>,
    failed: bool,
    diagnostics: FfmpegDiagnostics,
}

/// Diagnostic output collected from an ffmpeg process, printed when the
/// process fails so the underlying error is visible to the user.
struct FfmpegDiagnostics {
    errors: Vec<String>,
    tail: Vec<String>,
}

impl FfmpegDiagnostics {
    fn print(&self) {
        match (&self.errors[..], &self.tail[..]) {
            ([], []) => {}
            (errors, tail) => {
                let lines = if errors.is_empty() { tail } else { errors };
                for line in lines {
                    eprintln!("  {}", line.bright_black());
                }
            }
        }
    }
}

/// Ring buffer that keeps the last `capacity` log lines of any level and
/// separately records error/fatal lines.
struct LogCapture {
    errors: Vec<String>,
    tail: VecDeque<String>,
    capacity: usize,
}

impl LogCapture {
    fn new(capacity: usize) -> Self {
        Self {
            errors: Vec::new(),
            tail: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, level: LogLevel, line: &str) {
        if matches!(level, LogLevel::Error | LogLevel::Fatal) {
            self.errors.push(line.to_string());
        }
        if self.tail.len() == self.capacity {
            self.tail.pop_front();
        }
        self.tail.push_back(line.to_string());
    }

    fn finish(self) -> FfmpegDiagnostics {
        FfmpegDiagnostics {
            errors: self.errors,
            tail: self.tail.into_iter().collect(),
        }
    }
}

/// Streaming counter for the `crop=W:H:X:Y` values reported by ffmpeg's
/// cropdetect filter. Values are kept in first-seen order so that
/// `select_crop` can break ties deterministically.
struct CropCounter {
    order: Vec<String>,
    counts: HashMap<String, u32>,
}

impl CropCounter {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            counts: HashMap::new(),
        }
    }

    fn push(&mut self, line: &str) {
        static CROP_RE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"crop=(\d+:\d+:\d+:\d+)").unwrap());
        if let Some(capture) = CROP_RE.captures(line) {
            let value = capture[1].to_string();
            let count = self.counts.entry(value.clone()).or_insert(0);
            if *count == 0 {
                self.order.push(value.clone());
            }
            *count += 1;
        }
    }

    fn finish(self) -> Vec<(String, u32)> {
        self.order
            .into_iter()
            .map(|value| {
                let count = self.counts[&value];
                (value, count)
            })
            .collect()
    }
}

/// Selects the most frequently reported crop value; ties go to the value
/// observed first.
fn select_crop(values: &[(String, u32)]) -> Option<String> {
    let mut best: Option<(usize, u32)> = None;
    for (index, (_, count)) in values.iter().enumerate() {
        let replace = match best {
            None => true,
            Some((_, best_count)) => *count > best_count,
        };
        if replace {
            best = Some((index, *count));
        }
    }
    best.map(|(index, _)| values[index].0.clone())
}

fn is_full_frame(crop: &str, size: (u32, u32)) -> bool {
    crop == format!("{}:{}:0:0", size.0, size.1)
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    run()
}

fn run() -> color_eyre::Result<()> {
    let args = Args::parse();

    ensure_ffmpeg()?;

    let input_dir = resolve_existing_dir(&args.input, "Input")?;
    let output_dir = resolve_output_dir(&args.output)?;

    let video_files = find_video_files(&input_dir);
    let total = video_files.len();

    if total == 0 {
        println!(
            "{} No video files found in: {}",
            "WARNING:".yellow(),
            input_dir.display()
        );
        return Ok(());
    }

    println!("{}", "========================================".cyan());
    println!("{}", "Black Bar Removal".cyan().bold());
    println!("{}", "========================================".cyan());
    println!("Input:  {}", input_dir.display());
    println!("Output: {}", output_dir.display());
    println!("Found {total} video file(s) to process");
    println!("{}", "========================================".cyan());

    let (mut processed, mut skipped, mut failed) = (0, 0, 0);

    for (index, file) in video_files.iter().enumerate() {
        println!();
        println!(
            "{}",
            "----------------------------------------".bright_black()
        );
        println!(
            "{} {}",
            format!("[{}/{}]", index + 1, total).yellow(),
            file.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .yellow()
        );

        match process_file(file, &input_dir, &output_dir, &args) {
            Outcome::Processed => processed += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => failed += 1,
        }
    }

    println!();
    println!("{}", "========================================".cyan());
    println!("{}", "PROCESSING COMPLETE".green().bold());
    println!("{}", "========================================".cyan());
    println!("Processed: {processed}");
    println!("Skipped:   {skipped}");
    println!("Failed:    {failed}");
    println!("Total:     {total}");

    Ok(())
}

fn ensure_ffmpeg() -> color_eyre::Result<()> {
    let available = FfmpegCommand::new()
        .arg("-version")
        .spawn()
        .and_then(|mut child| child.wait())
        .map(|status| status.success())
        .unwrap_or(false);

    if !available {
        bail!("ffmpeg is not installed or not in PATH");
    }

    Ok(())
}

fn resolve_existing_dir(path: &Path, label: &str) -> color_eyre::Result<PathBuf> {
    if !path.exists() {
        bail!("{label} path does not exist: {}", path.display());
    }
    if !path.is_dir() {
        bail!("{label} path is not a directory: {}", path.display());
    }
    path.canonicalize()
        .wrap_err_with(|| format!("Failed to resolve {label} path: {}", path.display()))
}

fn resolve_output_dir(path: &Path) -> color_eyre::Result<PathBuf> {
    if !path.exists() {
        fs::create_dir_all(path)
            .wrap_err_with(|| format!("Failed to create output directory: {}", path.display()))?;
    }
    resolve_existing_dir(path, "Output")
}

fn find_video_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
        })
        .map(walkdir::DirEntry::into_path)
        .collect();
    files.sort();
    files
}

fn process_file(file: &Path, input_dir: &Path, output_dir: &Path, args: &Args) -> Outcome {
    let relative = match file.strip_prefix(input_dir) {
        Ok(relative) => relative,
        Err(err) => {
            eprintln!("{} {err:#}", "ERROR:".red().bold());
            return Outcome::Failed;
        }
    };
    let output_file = output_dir.join(relative);

    if let Some(parent) = output_file.parent()
        && !parent.exists()
    {
        if let Err(err) = fs::create_dir_all(parent) {
            eprintln!(
                "{} Failed to create directory {}: {err}",
                "ERROR:".red().bold(),
                parent.display()
            );
            return Outcome::Failed;
        }
        println!(
            "{}",
            format!("Created directory: {}", parent.display()).bright_black()
        );
    }

    if output_file.exists() {
        println!("{}", "SKIPPED: Output file already exists".bright_black());
        return Outcome::Skipped;
    }

    println!("{}", "Detecting black bars...".cyan());
    let detection = match detect_crop(file, args.crop_detect_seconds) {
        Ok(detection) => detection,
        Err(err) => {
            eprintln!("{} {err:#}", "ERROR:".red().bold());
            return Outcome::Failed;
        }
    };

    if detection.failed && detection.crop.is_none() {
        eprintln!(
            "{}",
            "FAILED: FFmpeg returned an error during crop detection"
                .red()
                .bold()
        );
        detection.diagnostics.print();
        return Outcome::Failed;
    }

    let is_full_frame = detection
        .crop
        .as_deref()
        .zip(detection.source_size)
        .is_some_and(|(crop, size)| is_full_frame(crop, size));

    let Some(crop) = detection.crop.filter(|_| !is_full_frame) else {
        println!("{}", "No black bars detected".yellow());
        println!("{}", "Copying file without modification...".cyan());
        return match fs::copy(file, &output_file) {
            Ok(_) => {
                println!("{}", "SUCCESS: Copied without changes".green());
                Outcome::Processed
            }
            Err(err) => {
                eprintln!("{} Failed to copy file: {err}", "ERROR:".red().bold());
                Outcome::Failed
            }
        };
    };

    let crop_filter = format!("crop={crop}");
    println!("{}", format!("Detected: {crop_filter}").green());
    println!("{}", "Encoding with crop filter...".cyan());

    match encode_video(file, &output_file, &crop_filter, args) {
        Ok(true) => {
            println!("{}", format!("SUCCESS: {}", relative.display()).green());
            Outcome::Processed
        }
        Ok(false) => {
            eprintln!("{}", "FAILED: FFmpeg returned an error".red().bold());
            let _ = fs::remove_file(&output_file);
            Outcome::Failed
        }
        Err(err) => {
            eprintln!("{} {err:#}", "ERROR:".red().bold());
            let _ = fs::remove_file(&output_file);
            Outcome::Failed
        }
    }
}

fn detect_crop(input: &Path, seconds: u64) -> color_eyre::Result<CropDetection> {
    let mut capture = LogCapture::new(10);
    let mut crop_counter = CropCounter::new();
    let mut source_size: Option<(u32, u32)> = None;

    let mut command = FfmpegCommand::new();
    command
        .hide_banner()
        .args(["-ss", &CROP_DETECT_START_SECS.to_string()])
        .input(input.to_string_lossy())
        .args(["-t", &seconds.to_string()])
        // `-vf` (and not sidecar's `.filter()`, which emits `-filter`) is
        // required so the filtergraph binds to video streams only; with bare
        // `-filter`, files that also contain audio/subtitle streams make
        // ffmpeg fail with "Filtergraph has a video output, cannot connect it
        // to audio output stream".
        .args(["-vf", CROP_DETECT_FILTER])
        .format("null")
        .output("-");

    if std::env::var("REMOVE_BARS_DEBUG").is_ok() {
        eprintln!(
            "DEBUG cropdetect argv: {:?}",
            command.get_args().collect::<Vec<_>>()
        );
    }

    let mut child = command
        .spawn()
        .wrap_err("Failed to start ffmpeg for crop detection")?;

    for event in child
        .iter()
        .map_err(|err| eyre!("Failed to read ffmpeg output: {err}"))?
    {
        match event {
            FfmpegEvent::ParsedInputStream(stream) => {
                if let Some(video) = stream.video_data() {
                    let size = (video.width, video.height);
                    source_size = Some(match source_size {
                        Some(largest) if largest.0 * largest.1 >= size.0 * size.1 => largest,
                        _ => size,
                    });
                }
            }
            FfmpegEvent::Log(level, line) => {
                capture.push(level, &line);
                crop_counter.push(&line);
            }
            _ => {}
        }
    }

    let succeeded = child
        .wait()
        .wrap_err("FFmpeg crop detection process failed")?
        .success();

    let crop = select_crop(&crop_counter.finish());

    Ok(CropDetection {
        crop,
        source_size,
        failed: !succeeded,
        diagnostics: capture.finish(),
    })
}

fn encode_video(
    input: &Path,
    output: &Path,
    crop_filter: &str,
    args: &Args,
) -> color_eyre::Result<bool> {
    let mut command = FfmpegCommand::new();
    command
        .hide_banner()
        .overwrite()
        .input(input.to_string_lossy())
        .args(["-vf", crop_filter])
        .map("0")
        .codec_video("libx264")
        .crf(args.crf)
        .preset(&args.preset)
        .codec_audio("copy")
        .codec_subtitle("copy")
        .output(output.to_string_lossy());

    if std::env::var("REMOVE_BARS_DEBUG").is_ok() {
        eprintln!(
            "DEBUG encode argv: {:?}",
            command.get_args().collect::<Vec<_>>()
        );
    }

    let mut child = command
        .spawn()
        .wrap_err("Failed to start ffmpeg for encoding")?;

    let mut capture = LogCapture::new(10);

    for event in child
        .iter()
        .map_err(|err| eyre!("Failed to read ffmpeg output: {err}"))?
    {
        match event {
            FfmpegEvent::Progress(progress) => {
                let line = format!(
                    "  time={} speed={:.2}x fps={:.0}",
                    progress.time, progress.speed, progress.fps
                );
                eprint!("\r{line:<70}");
            }
            FfmpegEvent::Log(level, line) => {
                if matches!(level, LogLevel::Error | LogLevel::Fatal) {
                    eprintln!("{line}");
                }
                capture.push(level, &line);
            }
            _ => {}
        }
    }
    eprintln!();

    let succeeded = child
        .wait()
        .wrap_err("FFmpeg encoding process failed")?
        .success();

    if !succeeded {
        capture.finish().print();
    }

    Ok(succeeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_counter_counts_values_across_lines() {
        let mut counter = CropCounter::new();
        counter.push("[Parsed_cropdetect_0 @ 0x0] x1:0 x2:1919 y1:138 y2:941 crop=1920:800:0:140");
        counter.push("[Parsed_cropdetect_0 @ 0x0] crop=1920:800:0:140");
        counter.push("[Parsed_cropdetect_0 @ 0x0] crop=1920:1080:0:0");

        let values = counter.finish();

        assert_eq!(
            values,
            vec![
                ("1920:800:0:140".to_string(), 2),
                ("1920:1080:0:0".to_string(), 1),
            ]
        );
    }

    #[test]
    fn crop_counter_ignores_lines_without_crop_values() {
        let mut counter = CropCounter::new();
        counter.push("frame= 1438 fps=0.0 q=-0.0 Lsize=N/A");
        counter.push("[out#0/null] video:595KiB audio:45000KiB");
        counter.push("At least one output file must be specified");

        assert!(counter.finish().is_empty());
    }

    #[test]
    fn select_crop_prefers_most_frequent_value() {
        let values = vec![
            ("1920:800:0:140".to_string(), 1),
            ("1920:1080:0:0".to_string(), 25),
        ];

        assert_eq!(select_crop(&values), Some("1920:1080:0:0".to_string()));
    }

    #[test]
    fn select_crop_breaks_ties_by_first_seen() {
        let values = vec![
            ("1920:800:0:140".to_string(), 3),
            ("1920:1080:0:0".to_string(), 3),
        ];

        assert_eq!(select_crop(&values), Some("1920:800:0:140".to_string()));
    }

    #[test]
    fn select_crop_returns_none_when_no_values() {
        assert_eq!(select_crop(&[]), None);
    }

    #[test]
    fn full_frame_crops_are_detected() {
        assert!(is_full_frame("1920:1080:0:0", (1920, 1080)));
        assert!(!is_full_frame("1920:800:0:140", (1920, 1080)));
        assert!(!is_full_frame("1920:1072:0:0", (1920, 1080)));
    }

    #[test]
    fn log_capture_keeps_only_the_last_lines() {
        let mut capture = LogCapture::new(3);
        for i in 0..5 {
            capture.push(LogLevel::Info, &format!("line {i}"));
        }

        let diagnostics = capture.finish();

        assert_eq!(
            diagnostics.tail,
            vec![
                "line 2".to_string(),
                "line 3".to_string(),
                "line 4".to_string()
            ]
        );
        assert!(diagnostics.errors.is_empty());
    }

    #[test]
    fn log_capture_records_error_lines() {
        let mut capture = LogCapture::new(10);
        capture.push(LogLevel::Info, "normal line");
        capture.push(LogLevel::Fatal, "fatal line");
        capture.push(LogLevel::Error, "error line");

        let diagnostics = capture.finish();

        assert_eq!(
            diagnostics.errors,
            vec!["fatal line".to_string(), "error line".to_string()]
        );
        assert_eq!(
            diagnostics.tail,
            vec![
                "normal line".to_string(),
                "fatal line".to_string(),
                "error line".to_string(),
            ]
        );
    }
}
