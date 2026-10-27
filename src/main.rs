use anyhow::{Context, Result, bail};
use cheek::download::Options;
use cheek::events::{Event, Reporter};
use cheek::extract::{self, ResolveOptions, Resolver};
use cheek::merge::Ffmpeg;
use cheek::{Job, Media, http};
use clap::Parser;
use std::io::Read;
use std::path::PathBuf;
use std::process::ExitCode;

/// Fast media downloads: parallel ranged HTTP, HLS/DASH fragments, ffmpeg merge.
/// Reads yt-dlp's JSON for the sites only yt-dlp knows.
#[derive(Parser)]
#[command(name = "cheek", version, about, long_about = None)]
struct Cli {
    /// The link to download.
    url: Option<String>,

    /// Take the media from yt-dlp's JSON (`yt-dlp -J`) instead of resolving; `-` reads stdin.
    #[arg(long, value_name = "FILE")]
    info_json: Option<PathBuf>,

    /// Print what would be downloaded as JSON and stop.
    #[arg(short = 'J', long)]
    dump_json: bool,

    /// yt-dlp format selector, used when yt-dlp resolves the link.
    #[arg(short, long, default_value = "bv*+ba/b")]
    format: String,

    /// Who resolves the link.
    #[arg(long, value_enum, default_value_t = Resolver::Auto)]
    resolver: Resolver,

    /// Path to yt-dlp (default: from PATH).
    #[arg(long, value_name = "PATH")]
    yt_dlp: Option<PathBuf>,

    /// Keep only the sound.
    #[arg(short = 'x', long)]
    extract_audio: bool,

    /// best (no re-encoding), opus, m4a, mp3, flac or wav.
    #[arg(long, default_value = "best")]
    audio_format: String,

    /// Container when video and audio are merged: mp4, mkv or webm.
    #[arg(long, value_name = "EXT")]
    merge_output_format: Option<String>,

    /// Highest video for HLS variants.
    #[arg(long, value_name = "PX")]
    max_height: Option<u32>,

    /// Output name, in yt-dlp's template syntax.
    #[arg(short, long, default_value = "%(title)s [%(id)s].%(ext)s")]
    output: String,

    /// Folder for finished files.
    #[arg(short = 'P', long, default_value = ".")]
    paths: PathBuf,

    /// Folder for unfinished `.part` files (default: the output folder).
    #[arg(long, value_name = "DIR")]
    temp_dir: Option<PathBuf>,

    /// Requests in flight across byte chunks or HLS/DASH fragments (shared by video and audio).
    #[arg(short = 'N', long, default_value_t = 8)]
    connections: usize,

    /// Largest byte range per request, e.g. 4M or 512K.
    #[arg(long, default_value = "8M", value_parser = parse_size)]
    chunk_size: u64,

    /// Path to ffmpeg (default: from PATH).
    #[arg(long, value_name = "PATH")]
    ffmpeg: Option<PathBuf>,

    /// Write the thumbnail into the file as its cover (mp4, m4a, mkv, mp3, opus, ogg, flac).
    #[arg(long)]
    embed_thumbnail: bool,

    /// Write title, artist and link into every file, even one that needed no ffmpeg step.
    #[arg(long)]
    embed_metadata: bool,

    /// Machine-readable events on stdout, one JSON object per line.
    #[arg(long)]
    json: bool,

    /// Only errors.
    #[arg(short, long)]
    quiet: bool,

    #[arg(long, value_name = "UA")]
    user_agent: Option<String>,
}

fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mult) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M') => (&s[..s.len() - 1], 1 << 20),
        Some('G') => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.trim()
        .parse::<f64>()
        .map(|n| (n * mult as f64) as u64)
        .map_err(|_| format!("not a size: {s}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let reporter = Reporter::new(cli.json, cli.quiet);
    tokio::select! {
        r = go(cli, &reporter) => match r {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                let msg = format!("{e:#}");
                reporter.emit(&Event::Error { message: &msg });
                ExitCode::FAILURE
            }
        },
        _ = tokio::signal::ctrl_c() => {
            // .part files and their .cheek notes stay; running the same command resumes.
            reporter.emit(&Event::Error { message: "interrupted; run it again to resume" });
            ExitCode::from(130)
        }
    }
}

async fn go(cli: Cli, reporter: &Reporter) -> Result<()> {
    let media: Media = match (&cli.info_json, &cli.url) {
        (Some(path), _) => {
            let text = if path.as_os_str() == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?
            };
            let value: serde_json::Value = serde_json::from_str(&text).context("the info JSON isn't valid JSON")?;
            // Either yt-dlp's info dict or cheek's own -J output.
            if value.get("streams").is_some() {
                serde_json::from_value(value)?
            } else {
                extract::ytdlp::from_info_json(&value)?
            }
        }
        (None, Some(url)) => {
            let client = http::page_client(cli.user_agent.as_deref())?;
            let opts = ResolveOptions {
                resolver: cli.resolver,
                yt_dlp: cli.yt_dlp.clone(),
                format: cli.format.clone(),
            };
            extract::resolve(&client, url, &opts).await?
        }
        (None, None) => bail!("give a link, or --info-json with yt-dlp's JSON"),
    };

    if cli.dump_json {
        println!("{}", serde_json::to_string_pretty(&media)?);
        return Ok(());
    }

    let job = Job {
        media,
        out_dir: cli.paths,
        temp_dir: cli.temp_dir,
        template: cli.output,
        audio: cli.extract_audio.then(|| cli.audio_format.to_ascii_lowercase()),
        merge_format: cli.merge_output_format,
        max_height: cli.max_height,
        ffmpeg: Ffmpeg::find(cli.ffmpeg.as_ref()),
        options: Options {
            connections: cli.connections.clamp(1, 64),
            chunk_size: cli.chunk_size.max(64 << 10),
        },
        embed_thumbnail: cli.embed_thumbnail,
        embed_metadata: cli.embed_metadata,
    };
    let client = http::download_client(cli.user_agent.as_deref(), job.options.connections)?;
    cheek::run(client, job, reporter).await?;
    Ok(())
}
