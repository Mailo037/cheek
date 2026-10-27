//! One download from start to finish: fetch every stream at once, then merge.

use crate::download::{self, Options, Shared};
use crate::events::{Event, Progress, Reporter};
use crate::hls;
use crate::media::{Media, Protocol, Stream, StreamKind};
use crate::merge::{Ffmpeg, audio_ext_for};
use crate::template;
use anyhow::{Context, Result, bail};
use reqwest::Client;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

pub struct Job {
    pub media: Media,
    pub out_dir: PathBuf,
    /// Where `.part` files live until they're done; the output folder by default.
    pub temp_dir: Option<PathBuf>,
    pub template: String,
    /// `Some(format)` keeps only the sound: `best` (no re-encode), `opus`, `m4a`, `mp3`, ...
    pub audio: Option<String>,
    pub merge_format: Option<String>,
    pub max_height: Option<u32>,
    pub ffmpeg: Option<Ffmpeg>,
    pub options: Options,
}

pub async fn run(client: Client, job: Job, reporter: &Reporter) -> Result<PathBuf> {
    let media = &job.media;
    let streams: Vec<&Stream> = match &job.audio {
        // For audio only, skip the picture when there is a separate sound stream.
        Some(_) if media.streams.iter().any(|s| s.kind == StreamKind::Audio) => {
            media.streams.iter().filter(|s| s.kind == StreamKind::Audio).collect()
        }
        _ => media.streams.iter().collect(),
    };
    if streams.is_empty() {
        bail!("nothing to download");
    }
    reporter.emit(&Event::Info {
        id: &media.id,
        title: &media.title,
        extractor: &media.extractor,
        streams: streams.len(),
        size: streams.iter().map(|s| s.size).sum(),
    });

    let ext = final_ext(&job, &streams);
    let final_path = job.out_dir.join(template::render(&job.template, media, &ext));
    // An empty leftover from a failed run doesn't count as done.
    if final_path.metadata().is_ok_and(|m| m.len() > 0) {
        reporter.emit(&Event::Stage {
            stage: "already downloaded",
        });
        let path = final_path.to_string_lossy();
        reporter.emit(&Event::Done {
            path: &path,
            bytes: std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0),
            seconds: 0.0,
        });
        return Ok(final_path);
    }
    if let Some(dir) = final_path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    let temp_dir = job
        .temp_dir
        .clone()
        .unwrap_or_else(|| final_path.parent().map(Path::to_path_buf).unwrap_or_default());
    tokio::fs::create_dir_all(&temp_dir).await?;
    let stem = final_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| media.id.clone());
    let parts: Vec<PathBuf> = streams
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let id = s.format_id.clone().unwrap_or_else(|| i.to_string());
            let ext = if s.protocol == Protocol::Hls { "ts" } else { s.ext.as_str() };
            temp_dir.join(format!("{stem}.f{id}.{ext}.part"))
        })
        .collect();

    let shared = Shared {
        client,
        permits: Arc::new(Semaphore::new(job.options.connections.max(1))),
        progress: Arc::new(Progress::default()),
        options: job.options.clone(),
    };
    let started = Instant::now();
    {
        let _watch = reporter.watch(shared.progress.clone());
        futures_util::future::try_join_all(streams.iter().zip(&parts).map(|(s, p)| {
            let shared = &shared;
            async move {
                match s.protocol {
                    Protocol::Http => download::fetch(shared, s, p).await,
                    Protocol::Hls => hls::fetch(shared, s, p, job.max_height).await,
                }
                .with_context(|| format!("downloading {}", s.format_id.as_deref().unwrap_or("the stream")))
            }
        }))
        .await?;
    }
    let (done, _) = shared.progress.snapshot();
    reporter.emit(&Event::Progress {
        downloaded: done,
        total: done,
        percent: 100.0,
        speed: 0,
        eta: Some(0),
    });

    let files: Vec<PathBuf> = parts.iter().map(|p| p.with_extension("")).collect();
    for (part, file) in parts.iter().zip(&files) {
        tokio::fs::rename(part, file).await?;
    }

    let need_ffmpeg = |why: &str| -> Result<&Ffmpeg> {
        job.ffmpeg
            .as_ref()
            .with_context(|| format!("ffmpeg is needed to {why}; install it or pass --ffmpeg"))
    };
    // ffmpeg writes next to the parts and the result moves into place at the end, so the
    // output folder never holds a half-written file (and ffmpeg never has to write to
    // storage it can't seek in, like Android's shared folders).
    // A plain name: ffmpeg reads `%` in output names as an image-sequence pattern.
    let merged = temp_dir.join(format!("cheek-merge-{}.{ext}", std::process::id()));
    if job.audio.is_some() {
        let source_ext = files[0].extension().and_then(|e| e.to_str()).unwrap_or("");
        if source_ext.eq_ignore_ascii_case(&ext) && streams[0].kind == StreamKind::Audio && streams[0].protocol == Protocol::Http {
            move_file(&files[0], &final_path).await?;
        } else {
            reporter.emit(&Event::Stage { stage: "extracting audio" });
            need_ffmpeg("extract the audio")?
                .audio(&files[0], streams[0].codec.as_deref(), &merged, media)
                .await?;
            move_file(&merged, &final_path).await?;
        }
    } else if files.len() > 1 {
        reporter.emit(&Event::Stage { stage: "merging" });
        let hls = streams.iter().any(|s| s.protocol == Protocol::Hls);
        need_ffmpeg("merge video and audio")?.mux(&files, &merged, media, hls).await?;
        move_file(&merged, &final_path).await?;
    } else if streams[0].protocol == Protocol::Hls && ext != "ts" {
        reporter.emit(&Event::Stage { stage: "remuxing" });
        need_ffmpeg("remux the HLS stream")?.mux(&files, &merged, media, true).await?;
        move_file(&merged, &final_path).await?;
    } else {
        move_file(&files[0], &final_path).await?;
    }
    for f in files.iter().chain([&merged]) {
        if f != &final_path {
            tokio::fs::remove_file(f).await.ok();
        }
    }

    let bytes = tokio::fs::metadata(&final_path).await.map(|m| m.len()).unwrap_or(0);
    let path = final_path.to_string_lossy();
    reporter.emit(&Event::Done {
        path: &path,
        bytes,
        seconds: started.elapsed().as_secs_f64(),
    });
    Ok(final_path)
}

/// The container of the finished file, chosen like yt-dlp does.
fn final_ext(job: &Job, streams: &[&Stream]) -> String {
    if let Some(format) = &job.audio {
        return if format == "best" {
            audio_ext_for(streams[0].codec.as_deref(), &streams[0].ext)
        } else {
            format.clone()
        };
    }
    if streams.len() > 1 {
        if let Some(f) = &job.merge_format {
            return f.clone();
        }
        let all = |set: &[&str]| streams.iter().all(|s| set.contains(&s.ext.as_str()));
        return if all(&["mp4", "m4a"]) {
            "mp4".into()
        } else if all(&["webm"]) {
            "webm".into()
        } else {
            "mkv".into()
        };
    }
    match streams[0].protocol {
        Protocol::Hls if job.ffmpeg.is_some() => job.merge_format.clone().unwrap_or_else(|| "mp4".into()),
        Protocol::Hls => "ts".into(),
        Protocol::Http => streams[0].ext.clone(),
    }
}

/// Rename, or copy and delete when the temp folder is on another drive.
async fn move_file(from: &Path, to: &Path) -> Result<()> {
    if tokio::fs::rename(from, to).await.is_err() {
        tokio::fs::copy(from, to)
            .await
            .with_context(|| format!("moving to {}", to.display()))?;
        tokio::fs::remove_file(from).await.ok();
    }
    Ok(())
}
