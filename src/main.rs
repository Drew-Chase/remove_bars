use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use color_eyre::eyre::{WrapErr, bail, eyre};
use colored::Colorize;
use ffmpeg_sidecar::{
    command::FfmpegCommand,
    event::{FfmpegEvent, LogLevel},
};
use indicatif::{ProgressBar, ProgressStyle};
use regex::Regex;
use rusqlite::{Connection, OpenFlags, params};
use walkdir::WalkDir;

/// Video encoder to use for the re-encode step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum VideoEncoder {
    /// Software x264 (CPU)
    #[value(name = "x264")]
    X264,
    /// NVIDIA NVENC hardware encoder (h264_nvenc)
    #[value(name = "nvenc")]
    Nvenc,
    /// AMD AMF hardware encoder (h264_amf)
    #[value(name = "amf")]
    Amf,
    /// Intel Quick Sync Video hardware encoder (h264_qsv)
    #[value(name = "qsv")]
    Qsv,
}

impl VideoEncoder {
    fn codec_name(self) -> &'static str {
        match self {
            VideoEncoder::X264 => "libx264",
            VideoEncoder::Nvenc => "h264_nvenc",
            VideoEncoder::Amf => "h264_amf",
            VideoEncoder::Qsv => "h264_qsv",
        }
    }
}

/// Detect and remove black bars (letterboxing/pillarboxing) from videos.
#[derive(Parser, Debug)]
#[command(
    version,
    about,
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Scan a directory and index which videos need cropping into an SQLite
    /// database, without modifying any files
    Scan {
        /// Directory containing the videos to scan
        #[arg(short, long, default_value = "input")]
        input: PathBuf,

        /// SQLite database file to write
        #[arg(short, long, default_value = "scan.sqlite")]
        output: PathBuf,

        /// Seconds of video to analyze, starting 30s into the file
        #[arg(short = 's', long, default_value_t = 60)]
        crop_detect_seconds: u64,
    },

    /// Crop videos in a directory, or videos indexed by a previous scan
    Crop {
        /// Directory of videos, or a scan database file
        #[arg(short, long, default_value = "input")]
        input: PathBuf,

        /// Directory to write processed videos to
        #[arg(short, long, default_value = "output")]
        output: PathBuf,

        #[command(flatten)]
        encode: EncodeArgs,
    },
}

/// Options that control how videos are cropped and encoded.
#[derive(clap::Args, Debug)]
struct EncodeArgs {
    /// Seconds of video to analyze, starting 30s into the file
    #[arg(short = 's', long, default_value_t = 60)]
    crop_detect_seconds: u64,

    /// Quality factor for the H.264 encode, 0-51 (lower = higher quality).
    /// CRF for x264, CQ for NVENC, global quality for QSV, QP for AMF.
    #[arg(short, long, default_value_t = 18)]
    crf: u32,

    /// Video encoder: x264 (CPU), nvenc (NVIDIA), amf (AMD) or qsv (Intel)
    #[arg(short = 'e', long, value_enum, default_value_t = VideoEncoder::X264)]
    encoder: VideoEncoder,

    /// x264-style encoding preset; hardware encoders map it to their own
    /// presets automatically
    #[arg(short, long, default_value = "medium")]
    preset: String,

    /// Copy files that need no cropping to the output directory instead of
    /// skipping them
    #[arg(long)]
    copy_uncropped: bool,

    /// Encode to a temporary file next to the original and replace the
    /// original only after a successful encode (-o/--output is ignored)
    #[arg(long, conflicts_with = "copy_uncropped")]
    overwrite_original: bool,
}

const VIDEO_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "mov", "wmv", "m4v", "webm"];

const CROP_DETECT_START_SECS: u64 = 30;

const CROP_DETECT_FILTER: &str = "cropdetect=24:16:0";

enum Outcome {
    Processed,
    Skipped,
    Failed,
}

/// Where encoded output goes: a mirrored output directory, or in-place
/// replacement of the original file.
enum OutputMode {
    Mirror(PathBuf),
    InPlace,
}

/// Where a file's crop value comes from: a live cropdetect run, or a previous
/// scan (`Some(crop)` = crop this value, `None` = scan found no bars).
enum CropSource {
    Detect,
    Scan(Option<String>),
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

/// Builds the ffmpeg output arguments for the video encoder, translating the
/// quality factor and x264-style preset to each encoder's own options.
fn video_encoder_args(encoder: VideoEncoder, crf: u32, preset: &str) -> Vec<String> {
    let crf = crf.to_string();
    match encoder {
        VideoEncoder::X264 => vec![
            "-c:v".into(),
            "libx264".into(),
            "-crf".into(),
            crf,
            "-preset".into(),
            preset.into(),
        ],
        VideoEncoder::Nvenc => vec![
            "-c:v".into(),
            "h264_nvenc".into(),
            "-rc".into(),
            "vbr".into(),
            "-cq".into(),
            crf,
            "-b:v".into(),
            "0".into(),
            "-preset".into(),
            nvenc_preset(preset).into(),
        ],
        VideoEncoder::Qsv => vec![
            "-c:v".into(),
            "h264_qsv".into(),
            "-global_quality".into(),
            crf,
            "-preset".into(),
            preset.into(),
        ],
        VideoEncoder::Amf => vec![
            "-c:v".into(),
            "h264_amf".into(),
            "-rc".into(),
            "cqp".into(),
            "-qp_i".into(),
            crf.clone(),
            "-qp_p".into(),
            crf,
            "-quality".into(),
            amf_quality(preset).into(),
        ],
    }
}

/// Maps x264-style presets to NVENC's p1 (fastest) through p7 (best quality)
/// presets. NVENC preset names (p1-p7) are passed through unchanged.
fn nvenc_preset(preset: &str) -> String {
    match preset {
        "ultrafast" => "p1",
        "superfast" => "p2",
        "veryfast" | "faster" | "fast" => "p3",
        "medium" => "p4",
        "slow" => "p5",
        "slower" => "p6",
        "veryslow" => "p7",
        other => return other.to_string(),
    }
    .to_string()
}

/// Maps x264-style presets to AMF's usage-quality presets.
fn amf_quality(preset: &str) -> String {
    match preset {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" => "speed",
        "medium" => "balanced",
        _ => "quality",
    }
    .to_string()
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let cli = Cli::parse();
    match cli.command {
        Command::Scan {
            input,
            output,
            crop_detect_seconds,
        } => run_scan(input, output, crop_detect_seconds),
        Command::Crop {
            input,
            output,
            encode,
        } => run_crop(input, output, encode),
    }
}

fn print_header() {
    println!("{}", "========================================".cyan());
    println!("{}", "Black Bar Removal".cyan().bold());
    println!("{}", "========================================".cyan());
}

fn print_output_target(output: &OutputMode) {
    match output {
        OutputMode::Mirror(dir) => println!("Output: {}", dir.display()),
        OutputMode::InPlace => println!("Mode:   in-place (originals are overwritten)"),
    }
}

fn print_summary(processed: usize, skipped: usize, failed: usize, total: usize) {
    println!();
    println!("{}", "========================================".cyan());
    println!("{}", "PROCESSING COMPLETE".green().bold());
    println!("{}", "========================================".cyan());
    println!("Processed: {processed}");
    println!("Skipped:   {skipped}");
    println!("Failed:    {failed}");
    println!("Total:     {total}");
}

fn run_scan(input: PathBuf, output: PathBuf, crop_detect_seconds: u64) -> color_eyre::Result<()> {
    ensure_ffmpeg()?;

    let input_dir = resolve_existing_dir(&input, "Input")?;
    let video_files = find_video_files(&input_dir);
    let total = video_files.len();

    print_header();
    println!("Input:    {}", input_dir.display());
    println!("Database: {}", output.display());
    println!("Found {total} video file(s) to scan");
    println!("{}", "========================================".cyan());

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        fs::create_dir_all(parent)
            .wrap_err_with(|| format!("Failed to create directory: {}", parent.display()))?;
    }

    let connection = Connection::open(&output)
        .with_context(|| format!("Failed to open scan database: {}", output.display()))?;
    connection
        .execute_batch(
            "DROP TABLE IF EXISTS videos;
             DROP TABLE IF EXISTS meta;
             CREATE TABLE meta (
                 key   TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             CREATE TABLE videos (
                 path       TEXT PRIMARY KEY,
                 crop       TEXT,
                 width      INTEGER,
                 height     INTEGER,
                 needs_crop INTEGER NOT NULL,
                 status     TEXT NOT NULL,
                 scanned_at INTEGER NOT NULL
             );",
        )
        .wrap_err("Failed to initialize scan database schema")?;

    let scanned_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default();

    let mut needs_crop = 0;
    let mut no_crop = 0;
    let mut errors = 0;

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

        let relative = match file.strip_prefix(&input_dir) {
            Ok(relative) => relative.to_string_lossy().replace('\\', "/"),
            Err(err) => {
                eprintln!("{} {err:#}", "ERROR:".red().bold());
                errors += 1;
                continue;
            }
        };

        let detection = match detect_crop(file, crop_detect_seconds) {
            Ok(detection) => detection,
            Err(err) => {
                eprintln!("{} {err:#}", "ERROR:".red().bold());
                errors += 1;
                insert_scan_row(
                    &connection,
                    &relative,
                    None,
                    None,
                    None,
                    false,
                    "error",
                    scanned_at,
                )?;
                continue;
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
            errors += 1;
            insert_scan_row(
                &connection,
                &relative,
                None,
                None,
                None,
                false,
                "error",
                scanned_at,
            )?;
            continue;
        }

        let is_full_frame = detection
            .crop
            .as_deref()
            .zip(detection.source_size)
            .is_some_and(|(crop, size)| is_full_frame(crop, size));

        let (crop, needs) = match &detection.crop {
            Some(value) if !is_full_frame => {
                println!("{}", format!("Needs cropping: crop={value}").green());
                needs_crop += 1;
                (Some(value.clone()), true)
            }
            Some(value) => {
                println!("{}", "No black bars detected".yellow());
                no_crop += 1;
                (Some(value.clone()), false)
            }
            None => {
                println!("{}", "No black bars detected".yellow());
                no_crop += 1;
                (None, false)
            }
        };
        let (width, height) = detection
            .source_size
            .map(|(w, h)| (Some(w), Some(h)))
            .unwrap_or((None, None));

        insert_scan_row(
            &connection,
            &relative,
            crop,
            width,
            height,
            needs,
            "scanned",
            scanned_at,
        )?;
    }

    connection
        .execute(
            "INSERT INTO meta (key, value) VALUES ('input_root', ?1)",
            params![input_dir.to_string_lossy()],
        )
        .wrap_err("Failed to write scan metadata")?;

    println!();
    println!("{}", "========================================".cyan());
    println!("{}", "SCAN COMPLETE".green().bold());
    println!("{}", "========================================".cyan());
    println!("Need cropping: {needs_crop}");
    println!("No crop:       {no_crop}");
    println!("Errors:        {errors}");
    println!("Total:         {total}");
    println!("Scan database saved: {}", output.display());

    Ok(())
}

fn insert_scan_row(
    connection: &Connection,
    path: &str,
    crop: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    needs_crop: bool,
    status: &str,
    scanned_at: i64,
) -> color_eyre::Result<()> {
    connection
        .execute(
            "INSERT INTO videos (path, crop, width, height, needs_crop, status, scanned_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                path,
                crop,
                width,
                height,
                needs_crop as i64,
                status,
                scanned_at
            ],
        )
        .with_context(|| format!("Failed to index {path}"))?;
    Ok(())
}

struct ScanEntry {
    path: String,
    crop: Option<String>,
    needs_crop: bool,
}

/// Reads the indexed videos and the scanned input root from a scan database.
fn read_scan_db(db_path: &Path) -> color_eyre::Result<(PathBuf, Vec<ScanEntry>)> {
    let connection = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("Failed to open scan database: {}", db_path.display()))?;

    let input_root: String = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'input_root'",
            [],
            |row| row.get(0),
        )
        .with_context(|| {
            format!(
                "Scan database has no input root; was {} created by 'remove_bars scan'?",
                db_path.display()
            )
        })?;

    let mut statement = connection
        .prepare("SELECT path, crop, needs_crop FROM videos ORDER BY path")
        .with_context(|| format!("Failed to read scan database: {}", db_path.display()))?;
    let entries = statement
        .query_map([], |row| {
            Ok(ScanEntry {
                path: row.get(0)?,
                crop: row.get(1)?,
                needs_crop: row.get::<_, i64>(2)? != 0,
            })
        })
        .wrap_err("Failed to read scan database")?
        .collect::<Result<Vec<_>, _>>()
        .wrap_err("Failed to read scan database")?;

    Ok((PathBuf::from(input_root), entries))
}

fn run_crop(input: PathBuf, output: PathBuf, encode: EncodeArgs) -> color_eyre::Result<()> {
    ensure_ffmpeg()?;
    ensure_encoder(encode.encoder)?;

    let output_mode = if encode.overwrite_original {
        OutputMode::InPlace
    } else {
        OutputMode::Mirror(resolve_output_dir(&output)?)
    };

    struct CropJob {
        file: PathBuf,
        relative: PathBuf,
        source: CropSource,
    }

    let mut jobs: Vec<CropJob> = Vec::new();

    print_header();
    if input.is_dir() {
        let input_dir = resolve_existing_dir(&input, "Input")?;
        let video_files = find_video_files(&input_dir);
        println!("Input:  {}", input_dir.display());
        print_output_target(&output_mode);
        println!("Found {} video file(s) to process", video_files.len());
        println!("{}", "========================================".cyan());

        for file in video_files {
            let relative = file
                .strip_prefix(&input_dir)
                .wrap_err("Failed to compute relative path")?
                .to_path_buf();
            jobs.push(CropJob {
                file,
                relative,
                source: CropSource::Detect,
            });
        }
    } else if input.is_file() {
        if !input.exists() {
            bail!("Scan database not found: {}", input.display());
        }
        let (input_root, entries) = read_scan_db(&input)?;
        let input_root = input_root.canonicalize().with_context(|| {
            format!(
                "Scanned input root no longer exists: {}",
                input_root.display()
            )
        })?;
        println!("Scan:   {}", input.display());
        println!("Input:  {}", input_root.display());
        print_output_target(&output_mode);
        let needs_crop = entries.iter().filter(|entry| entry.needs_crop).count();
        println!(
            "Indexed {} video file(s), {} need cropping",
            entries.len(),
            needs_crop
        );
        println!("{}", "========================================".cyan());

        for entry in entries {
            let source = if entry.needs_crop {
                match entry.crop {
                    Some(crop) => CropSource::Scan(Some(crop)),
                    None => CropSource::Detect,
                }
            } else {
                CropSource::Scan(None)
            };
            jobs.push(CropJob {
                file: input_root.join(&entry.path),
                relative: PathBuf::from(&entry.path),
                source,
            });
        }
    } else if !input.exists() {
        bail!("Input path does not exist: {}", input.display());
    } else {
        bail!(
            "Input path is neither a directory nor a scan database: {}",
            input.display()
        );
    }

    if jobs.is_empty() {
        println!("{} No video files found", "WARNING:".yellow());
        return Ok(());
    }

    let total = jobs.len();
    let (mut processed, mut skipped, mut failed) = (0, 0, 0);

    for (index, job) in jobs.iter().enumerate() {
        println!();
        println!(
            "{}",
            "----------------------------------------".bright_black()
        );
        println!(
            "{} {}",
            format!("[{}/{}]", index + 1, total).yellow(),
            job.file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .yellow()
        );

        if !job.file.exists() {
            println!(
                "{}",
                format!(
                    "SKIPPED: file not found (moved or renamed since the scan): {}",
                    job.file.display()
                )
                .bright_black()
            );
            skipped += 1;
            continue;
        }

        match process_file(&job.file, &job.relative, &output_mode, &encode, &job.source) {
            Outcome::Processed => processed += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::Failed => failed += 1,
        }
    }

    print_summary(processed, skipped, failed, total);

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

/// Verifies the selected encoder is compiled into the system's ffmpeg build
/// before processing any files. Note that this cannot detect a missing GPU or
/// driver; that only surfaces when the first encode runs.
fn ensure_encoder(encoder: VideoEncoder) -> color_eyre::Result<()> {
    let codec = encoder.codec_name();

    let mut child = FfmpegCommand::new()
        .arg("-encoders")
        .spawn()
        .wrap_err("Failed to start ffmpeg")?;

    let mut list = String::new();
    if let Some(mut stdout) = child.take_stdout() {
        stdout
            .read_to_string(&mut list)
            .wrap_err("Failed to read ffmpeg encoder list")?;
    }
    child
        .wait()
        .wrap_err("FFmpeg encoder list process failed")?;

    let available = list
        .lines()
        .any(|line| line.split_whitespace().any(|token| token == codec));

    if !available {
        bail!(
            "Encoder {codec} is not available in this ffmpeg build; \
             use '-e x264' or install an ffmpeg build with {codec} support"
        );
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

fn process_file(
    file: &Path,
    relative: &Path,
    output: &OutputMode,
    encode: &EncodeArgs,
    source: &CropSource,
) -> Outcome {
    let in_place = matches!(output, OutputMode::InPlace);
    let output_file = match output {
        OutputMode::Mirror(dir) => dir.join(relative),
        OutputMode::InPlace => temp_output_path(file),
    };

    if let OutputMode::Mirror(dir) = output {
        let output_file = dir.join(relative);
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
    }

    let crop: Option<String> = match source {
        CropSource::Scan(Some(value)) => {
            println!("{}", format!("Using scan result: crop={value}").green());
            Some(value.clone())
        }
        CropSource::Scan(None) => None,
        CropSource::Detect => {
            println!("{}", "Detecting black bars...".cyan());
            let detection = match detect_crop(file, encode.crop_detect_seconds) {
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

            detection.crop.filter(|_| !is_full_frame)
        }
    };

    let Some(crop) = crop else {
        if in_place {
            println!(
                "{}",
                "No black bars detected - nothing to do, file left as is".yellow()
            );
            return Outcome::Skipped;
        }
        if !encode.copy_uncropped {
            println!(
                "{}",
                "No black bars detected - skipping (use --copy-uncropped to copy unchanged files)"
                    .yellow()
            );
            return Outcome::Skipped;
        }

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

    match encode_video(file, &output_file, &crop_filter, encode) {
        Ok(true) => {
            if in_place {
                if let Err(err) = replace_file(&output_file, file) {
                    eprintln!(
                        "{} Failed to replace original {}: {err:#}",
                        "ERROR:".red().bold(),
                        file.display()
                    );
                    let _ = fs::remove_file(&output_file);
                    return Outcome::Failed;
                }
                println!(
                    "{}",
                    format!("SUCCESS: {} (original replaced)", relative.display()).green()
                );
            } else {
                println!("{}", format!("SUCCESS: {}", relative.display()).green());
            }
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

/// Temporary encode target for in-place processing: lives next to the
/// original (same filesystem, so replacing it is a rename) and keeps the
/// original's extension so ffmpeg picks the same muxer.
fn temp_output_path(original: &Path) -> PathBuf {
    let stem = original.file_stem().unwrap_or_default().to_string_lossy();
    let extension = original
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    original.with_file_name(format!(
        "{stem}.remove-bars-{}{extension}",
        std::process::id()
    ))
}

/// Replaces `original` with `replacement` via rename, falling back to
/// copy-and-delete when the paths live on different filesystems.
fn replace_file(replacement: &Path, original: &Path) -> color_eyre::Result<()> {
    if fs::rename(replacement, original).is_ok() {
        return Ok(());
    }
    fs::copy(replacement, original).wrap_err_with(|| {
        format!(
            "Failed to copy {} over {}",
            replacement.display(),
            original.display()
        )
    })?;
    fs::remove_file(replacement)
        .wrap_err_with(|| format!("Failed to remove {}", replacement.display()))?;
    Ok(())
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
    encode: &EncodeArgs,
) -> color_eyre::Result<bool> {
    let mut command = FfmpegCommand::new();
    command
        .hide_banner()
        .overwrite()
        .input(input.to_string_lossy())
        .args(["-vf", crop_filter])
        .map("0")
        .args(video_encoder_args(
            encode.encoder,
            encode.crf,
            &encode.preset,
        ))
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
    let mut duration_secs: Option<f64> = None;
    let mut fps: Option<f32> = None;
    let mut bar: Option<ProgressBar> = None;
    let file_name = output
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    for event in child
        .iter()
        .map_err(|err| eyre!("Failed to read ffmpeg output: {err}"))?
    {
        match event {
            FfmpegEvent::ParsedDuration(parsed) => duration_secs = Some(parsed.duration),
            FfmpegEvent::ParsedInputStream(stream) => {
                if let Some(video) = stream.video_data() {
                    fps = Some(video.fps);
                }
            }
            FfmpegEvent::Progress(progress) => {
                let bar = bar.get_or_insert_with(|| {
                    create_progress_bar(&file_name, estimated_total_frames(duration_secs, fps))
                });
                bar.set_position(progress.frame as u64);
                bar.set_message(if progress.fps > 0.0 {
                    format!(
                        "{file_name} ({:.2}x, {:.0} fps)",
                        progress.speed, progress.fps
                    )
                } else {
                    format!("{file_name} ({:.2}x)", progress.speed)
                });
            }
            FfmpegEvent::Log(level, line) => {
                if matches!(level, LogLevel::Error | LogLevel::Fatal) {
                    match &bar {
                        Some(bar) => bar.println(&line),
                        None => eprintln!("{line}"),
                    }
                }
                capture.push(level, &line);
            }
            _ => {}
        }
    }

    if let Some(bar) = bar {
        bar.finish_and_clear();
    }

    let succeeded = child
        .wait()
        .wrap_err("FFmpeg encoding process failed")?
        .success();

    if !succeeded {
        capture.finish().print();
    }

    Ok(succeeded)
}

/// Estimates the total number of output frames from the input duration and
/// video framerate, both parsed from ffmpeg's input metadata.
fn estimated_total_frames(duration_secs: Option<f64>, fps: Option<f32>) -> Option<u64> {
    let total = duration_secs? * fps? as f64;
    (total > 0.0).then(|| total.round() as u64)
}

fn create_progress_bar(file_name: &str, total_frames: Option<u64>) -> ProgressBar {
    match total_frames {
        Some(total) => {
            let bar = ProgressBar::new(total);
            bar.set_style(
                ProgressStyle::with_template(
                    "Encoding {msg} [{wide_bar:.cyan/blue}] {percent:>3}% {pos}/{len} frames eta {eta}",
                )
                .expect("progress bar template is valid")
                .progress_chars("█░"),
            );
            bar.set_message(file_name.to_string());
            bar
        }
        None => {
            let bar = ProgressBar::new_spinner();
            bar.set_style(
                ProgressStyle::with_template("Encoding {msg} {pos} frames")
                    .expect("progress bar template is valid"),
            );
            bar.set_message(file_name.to_string());
            bar.enable_steady_tick(std::time::Duration::from_millis(200));
            bar
        }
    }
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

    #[test]
    fn nvenc_preset_presets_are_mapped_monotonically() {
        assert_eq!(nvenc_preset("ultrafast"), "p1");
        assert_eq!(nvenc_preset("veryfast"), "p3");
        assert_eq!(nvenc_preset("medium"), "p4");
        assert_eq!(nvenc_preset("slow"), "p5");
        assert_eq!(nvenc_preset("veryslow"), "p7");
    }

    #[test]
    fn nvenc_preset_passes_through_unknown_names() {
        assert_eq!(nvenc_preset("p2"), "p2");
        assert_eq!(nvenc_preset("llhq"), "llhq");
    }

    #[test]
    fn amf_quality_presets_are_mapped() {
        assert_eq!(amf_quality("ultrafast"), "speed");
        assert_eq!(amf_quality("fast"), "speed");
        assert_eq!(amf_quality("medium"), "balanced");
        assert_eq!(amf_quality("slow"), "quality");
        assert_eq!(amf_quality("veryslow"), "quality");
    }

    #[test]
    fn encoder_args_for_x264() {
        assert_eq!(
            video_encoder_args(VideoEncoder::X264, 18, "medium"),
            vec![
                "-c:v".to_string(),
                "libx264".to_string(),
                "-crf".to_string(),
                "18".to_string(),
                "-preset".to_string(),
                "medium".to_string(),
            ]
        );
    }

    #[test]
    fn encoder_args_for_nvenc() {
        assert_eq!(
            video_encoder_args(VideoEncoder::Nvenc, 18, "medium"),
            vec![
                "-c:v".to_string(),
                "h264_nvenc".to_string(),
                "-rc".to_string(),
                "vbr".to_string(),
                "-cq".to_string(),
                "18".to_string(),
                "-b:v".to_string(),
                "0".to_string(),
                "-preset".to_string(),
                "p4".to_string(),
            ]
        );
    }

    #[test]
    fn encoder_args_for_qsv() {
        assert_eq!(
            video_encoder_args(VideoEncoder::Qsv, 20, "slow"),
            vec![
                "-c:v".to_string(),
                "h264_qsv".to_string(),
                "-global_quality".to_string(),
                "20".to_string(),
                "-preset".to_string(),
                "slow".to_string(),
            ]
        );
    }

    #[test]
    fn encoder_args_for_amf() {
        assert_eq!(
            video_encoder_args(VideoEncoder::Amf, 22, "veryslow"),
            vec![
                "-c:v".to_string(),
                "h264_amf".to_string(),
                "-rc".to_string(),
                "cqp".to_string(),
                "-qp_i".to_string(),
                "22".to_string(),
                "-qp_p".to_string(),
                "22".to_string(),
                "-quality".to_string(),
                "quality".to_string(),
            ]
        );
    }

    #[test]
    fn encoder_codec_names_match_value_names() {
        assert_eq!(VideoEncoder::X264.codec_name(), "libx264");
        assert_eq!(VideoEncoder::Nvenc.codec_name(), "h264_nvenc");
        assert_eq!(VideoEncoder::Amf.codec_name(), "h264_amf");
        assert_eq!(VideoEncoder::Qsv.codec_name(), "h264_qsv");
    }
}
