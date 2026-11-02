//! One download from start to finish: fetch every stream at once, then merge.

use crate::download::{self, Options, Shared};
use crate::events::{Event, Progress, Reporter};
use crate::hls;
use crate::media::{Media, Protocol, Stream, StreamKind, is_audio_ext};
use crate::merge::{Ffmpeg, SubFile, audio_ext_for, subtitle_codec, taggable};
use crate::template;
use anyhow::{Context, Result, bail};
use reqwest::Client;
use std::cell::Cell;
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
    /// Write the thumbnail into the file as its cover (needs ffmpeg).
    pub embed_thumbnail: bool,
    /// Write title, artist and link even into files that needed no other ffmpeg step.
    pub embed_metadata: bool,
    /// Subtitle languages to write into the file (`en`, `de`, `all`); empty for none.
    pub subtitles: Vec<String>,
    /// The playlist this file is part of, for the done event.
    pub playlist_title: Option<String>,
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
    let done = |bytes: u64, seconds: f64| {
        let path = final_path.to_string_lossy();
        reporter.emit(&Event::Done {
            path: &path,
            bytes,
            seconds,
            title: Some(&media.title),
            thumbnail: media.thumbnail.as_deref(),
            duration: media.duration,
            playlist_title: job.playlist_title.as_deref(),
        });
    };
    // An empty leftover from a failed run doesn't count as done.
    if final_path.metadata().is_ok_and(|m| m.len() > 0) {
        reporter.emit(&Event::Stage {
            stage: "already downloaded",
        });
        done(std::fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0), 0.0);
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
    // A stream loses its `.part` once all of it is in; that file is kept until the result is.
    let files: Vec<PathBuf> = parts.iter().map(|p| p.with_extension("")).collect();

    let shared = Shared {
        client,
        permits: Arc::new(Semaphore::new(if job.options.adaptive {
            job.options.connections.clamp(1, 2)
        } else {
            job.options.connections.clamp(1, 64)
        })),
        progress: Arc::new(Progress::default()),
        options: job.options.clone(),
    };
    let governor = crate::adaptive::watch(&shared);
    let started = Instant::now();
    let thumbnail = temp_dir.join(format!("cheek-cover-{}", std::process::id()));
    let fetch_cover = async {
        let url = media.thumbnail.as_deref().filter(|_| job.embed_thumbnail && job.ffmpeg.is_some())?;
        let cover_started = Instant::now();
        // A missing cover is no reason to fail the download.
        let get = async { anyhow::Ok(shared.client.get(url).send().await?.error_for_status()?.bytes().await?) };
        let result = tokio::time::timeout(std::time::Duration::from_secs(20), get).await;
        reporter.emit(&Event::Timing {
            phase: "cover",
            seconds: cover_started.elapsed().as_secs_f64(),
        });
        let bytes = result.ok()?.ok()?;
        tokio::fs::write(&thumbnail, &bytes).await.ok()?;
        Some(())
    };
    // Subtitles go in where the container keeps them and ffmpeg is there to write them.
    let sub_files = async {
        let wanted: Vec<_> = if job.audio.is_none() && job.ffmpeg.is_some() && subtitle_codec(&ext).is_some() {
            media
                .subtitles
                .iter()
                .filter(|s| job.subtitles.iter().any(|w| s.matches(w)))
                .collect()
        } else {
            Vec::new()
        };
        let mut out = Vec::new();
        for (i, sub) in wanted.into_iter().enumerate() {
            let path = temp_dir.join(format!("cheek-sub-{}-{i}.{}", std::process::id(), sub.ext));
            let get = async { anyhow::Ok(shared.client.get(&sub.url).send().await?.error_for_status()?.bytes().await?) };
            // Like the cover: a subtitle that won't come is no reason to fail the download.
            if let Ok(Ok(bytes)) = tokio::time::timeout(std::time::Duration::from_secs(20), get).await
                && tokio::fs::write(&path, &bytes).await.is_ok()
            {
                out.push(SubFile {
                    path,
                    lang: sub.lang.clone(),
                });
            }
        }
        out
    };
    let (downloaded, got_cover, subs) = {
        let _watch = reporter.watch(shared.progress.clone());
        let downloads = futures_util::future::try_join_all(streams.iter().zip(&parts).zip(&files).map(|((s, p), f)| {
            let shared = &shared;
            async move {
                // Already complete from an earlier run whose merge failed: no need to fetch it again.
                if let Ok(m) = tokio::fs::metadata(f).await
                    && m.len() > 0
                {
                    shared.progress.add_total(m.len());
                    shared.progress.add(m.len());
                    return anyhow::Ok(m.len());
                }
                let size = match s.protocol {
                    Protocol::Http => download::fetch(shared, s, p).await,
                    Protocol::Hls => hls::fetch(shared, s, p, job.max_height).await,
                    Protocol::Fragments => hls::fetch_parts(shared, s, p, s.fragments.clone()).await,
                }
                .with_context(|| format!("downloading {}", s.format_id.as_deref().unwrap_or("the stream")))?;
                tokio::fs::rename(p, f).await?;
                anyhow::Ok(size)
            }
        }));
        let timed_downloads = async {
            let start = Instant::now();
            let result = downloads.await;
            reporter.emit(&Event::Timing {
                phase: "download",
                seconds: start.elapsed().as_secs_f64(),
            });
            result
        };
        tokio::join!(timed_downloads, fetch_cover, sub_files)
    };
    drop(governor);
    let post_started = Instant::now();
    let covers = [thumbnail.clone(), thumbnail.with_extension("jpg")];
    let extras = || covers.iter().chain(subs.iter().map(|s| &s.path));
    if let Err(e) = downloaded {
        remove_all(extras()).await;
        return Err(e);
    }
    let (fetched, _) = shared.progress.snapshot();
    reporter.emit(&Event::Progress {
        downloaded: fetched,
        total: fetched,
        percent: 100.0,
        speed: 0,
        eta: Some(0),
    });

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
    let cover = match (&job.ffmpeg, got_cover) {
        (Some(ffmpeg), Some(())) => ffmpeg.cover_for(&thumbnail, &ext).await.ok(),
        _ => None,
    };
    let cover = cover.as_ref();
    // How far ffmpeg is, as a share of the media's length, at most every half percent.
    let stage = Cell::new("");
    let shown = Cell::new(-1.0f64);
    let report = |seconds: f64| {
        let Some(total) = media.duration.filter(|d| *d > 0.0) else { return };
        let percent = ((seconds / total) * 100.0).clamp(0.0, 100.0);
        if percent - shown.get() >= 0.5 {
            shown.set(percent);
            reporter.emit(&Event::StageProgress {
                stage: stage.get(),
                percent: (percent * 10.0).round() / 10.0,
            });
        }
    };
    let progress: Option<&dyn Fn(f64)> = Some(&report);
    let begin = |name: &'static str| {
        stage.set(name);
        shown.set(-1.0);
        reporter.emit(&Event::Stage { stage: name });
    };
    let result: Result<()> = async {
        // Files that need no other ffmpeg step still get their tags, cover and subtitles when asked to.
        let retag = |video: bool| {
            let ffmpeg = job
                .ffmpeg
                .as_ref()
                .filter(|_| (job.embed_metadata || cover.is_some() || !subs.is_empty()) && taggable(&ext));
            let (files, merged, final_path, subs) = (&files, &merged, &final_path, &subs);
            async move {
                match ffmpeg {
                    Some(ffmpeg) => {
                        begin("tagging");
                        ffmpeg.tag(&files[0], merged, media, video, cover, subs, progress).await?;
                        move_file(merged, final_path).await
                    }
                    None => move_file(&files[0], final_path).await,
                }
            }
        };
        if job.audio.is_some() {
            let source_ext = files[0].extension().and_then(|e| e.to_str()).unwrap_or("");
            if source_ext.eq_ignore_ascii_case(&ext) && streams[0].kind == StreamKind::Audio && streams[0].protocol != Protocol::Hls {
                retag(false).await?;
            } else {
                begin("extracting audio");
                need_ffmpeg("extract the audio")?
                    .audio(&files[0], streams[0].codec.as_deref(), &merged, media, cover, progress)
                    .await?;
                move_file(&merged, &final_path).await?;
            }
        } else if files.len() > 1 {
            begin("merging");
            let hls = streams.iter().any(|s| s.protocol == Protocol::Hls);
            need_ffmpeg("merge video and audio")?
                .mux(&files, &merged, media, hls, cover, &subs, progress)
                .await?;
            move_file(&merged, &final_path).await?;
        } else if streams[0].protocol == Protocol::Hls && ext != "ts" {
            begin("remuxing");
            need_ffmpeg("remux the HLS stream")?
                .mux(&files, &merged, media, true, cover, &subs, progress)
                .await?;
            move_file(&merged, &final_path).await?;
        } else {
            retag(streams[0].kind != StreamKind::Audio && !is_audio_ext(&ext)).await?;
        }
        Ok(())
    }
    .await;
    remove_all(extras()).await;
    if let Err(e) = result {
        // The finished streams stay, so the next run only does this step again.
        tokio::fs::remove_file(&merged).await.ok();
        return Err(e);
    }
    for f in files.iter().chain([&merged]) {
        if f != &final_path {
            tokio::fs::remove_file(f).await.ok();
        }
    }

    reporter.emit(&Event::Timing {
        phase: "postprocess",
        seconds: post_started.elapsed().as_secs_f64(),
    });
    let bytes = tokio::fs::metadata(&final_path).await.map(|m| m.len()).unwrap_or(0);
    done(bytes, started.elapsed().as_secs_f64());
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
        Protocol::Http | Protocol::Fragments => streams[0].ext.clone(),
    }
}

async fn remove_all(paths: impl IntoIterator<Item = &PathBuf>) {
    for f in paths {
        tokio::fs::remove_file(f).await.ok();
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
