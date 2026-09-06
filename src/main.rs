use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
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
        return Outcome::Failed;
    }

    let is_full_frame = detection
        .crop
        .as_deref()
        .zip(detection.source_size)
        .is_some_and(|(crop, (width, height))| crop == format!("{width}:{height}:0:0"));

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
    let crop_re = Regex::new(r"crop=(\d+:\d+:\d+:\d+)").unwrap();
    let mut counts: HashMap<String, u32> = HashMap::new();
    let mut source_size: Option<(u32, u32)> = None;

    let mut child = FfmpegCommand::new()
        .hide_banner()
        .args(["-ss", &CROP_DETECT_START_SECS.to_string()])
        .input(input.to_string_lossy())
        .args(["-t", &seconds.to_string()])
        .filter(CROP_DETECT_FILTER)
        .format("null")
        .output("-")
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
            FfmpegEvent::Log(_, line) => {
                if let Some(capture) = crop_re.captures(&line) {
                    *counts.entry(capture[1].to_string()).or_default() += 1;
                }
            }
            _ => {}
        }
    }

    let succeeded = child
        .wait()
        .wrap_err("FFmpeg crop detection process failed")?
        .success();

    let crop = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(value, _)| value);

    Ok(CropDetection {
        crop,
        source_size,
        failed: !succeeded,
    })
}

fn encode_video(
    input: &Path,
    output: &Path,
    crop_filter: &str,
    args: &Args,
) -> color_eyre::Result<bool> {
    let mut child = FfmpegCommand::new()
        .hide_banner()
        .overwrite()
        .input(input.to_string_lossy())
        .filter(crop_filter)
        .map("0")
        .codec_video("libx264")
        .crf(args.crf)
        .preset(&args.preset)
        .codec_audio("copy")
        .codec_subtitle("copy")
        .output(output.to_string_lossy())
        .spawn()
        .wrap_err("Failed to start ffmpeg for encoding")?;

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
            FfmpegEvent::Log(LogLevel::Error | LogLevel::Fatal, line) => {
                eprintln!("{line}");
            }
            _ => {}
        }
    }
    eprintln!();

    Ok(child
        .wait()
        .wrap_err("FFmpeg encoding process failed")?
        .success())
}
