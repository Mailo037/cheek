//! ffmpeg for the last step: joining video and audio, pulling out the sound, remuxing HLS.
//! Streams are copied whenever the container allows it; re-encoding only happens for an
//! audio format the source doesn't have.

use crate::extract::which;
use crate::media::Media;
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Clone)]
pub struct Ffmpeg {
    bin: PathBuf,
}

impl Ffmpeg {
    pub fn find(explicit: Option<&PathBuf>) -> Option<Ffmpeg> {
        explicit.cloned().or_else(|| which("ffmpeg")).map(|bin| Ffmpeg { bin })
    }

    async fn run(&self, args: &[&std::ffi::OsStr]) -> Result<()> {
        let out = tokio::process::Command::new(&self.bin)
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| format!("running {}", self.bin.display()))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            bail!(
                "ffmpeg failed: {}",
                err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output")
            );
        }
        Ok(())
    }

    /// Copies the streams of all inputs into `out`, video from the first, audio from the last.
    /// `from_hls` marks MPEG-TS input, whose AAC needs repacking for MP4.
    pub async fn mux(&self, inputs: &[PathBuf], out: &Path, media: &Media, from_hls: bool) -> Result<()> {
        let mut args: Vec<std::ffi::OsString> = Vec::new();
        for i in inputs {
            args.extend(["-i".into(), i.into()]);
        }
        if inputs.len() > 1 {
            args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                format!("{}:a:0", inputs.len() - 1).into(),
            ]);
        }
        args.extend(["-c".into(), "copy".into()]);
        let ext = ext_of(out);
        if ext == "mp4" || ext == "m4a" || ext == "mov" {
            // HLS transport streams carry AAC in ADTS frames, which MP4 can't hold as they are.
            // Only then: the filter refuses other codecs, like the Opus YouTube often serves.
            if from_hls {
                args.extend(["-bsf:a".into(), "aac_adtstoasc".into()]);
            }
            args.extend(["-movflags".into(), "+faststart".into()]);
        }
        args.extend(metadata(media));
        args.push(out.into());
        let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        self.run(&refs).await
    }

    /// Writes the audio of `input` as `out`, copying when the codec already fits.
    pub async fn audio(&self, input: &Path, codec: Option<&str>, out: &Path, media: &Media) -> Result<()> {
        let ext = ext_of(out);
        let codec = codec.unwrap_or("").to_ascii_lowercase();
        let encode: &[&str] = match ext.as_str() {
            "opus" if codec.starts_with("opus") => &["-c:a", "copy"],
            "opus" => &["-c:a", "libopus", "-b:a", "160k"],
            "m4a" | "aac" if codec.starts_with("mp4a") || codec.starts_with("aac") => &["-c:a", "copy"],
            "m4a" | "aac" => &["-c:a", "aac", "-b:a", "192k"],
            "mp3" if codec.starts_with("mp3") => &["-c:a", "copy"],
            "mp3" => &["-c:a", "libmp3lame", "-q:a", "0"],
            "flac" => &["-c:a", "flac"],
            "wav" => &["-c:a", "pcm_s16le"],
            _ => &["-c:a", "copy"],
        };
        let mut args: Vec<std::ffi::OsString> = vec!["-i".into(), input.into(), "-vn".into()];
        args.extend(encode.iter().map(Into::into));
        args.extend(metadata(media));
        args.push(out.into());
        let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
        self.run(&refs).await
    }
}

fn metadata(media: &Media) -> Vec<std::ffi::OsString> {
    let mut v: Vec<std::ffi::OsString> = vec!["-metadata".into(), format!("title={}", media.title).into()];
    if let Some(u) = &media.uploader {
        v.extend(["-metadata".into(), format!("artist={u}").into()]);
    }
    if !media.webpage_url.is_empty() {
        v.extend(["-metadata".into(), format!("comment={}", media.webpage_url).into()]);
    }
    v
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase()
}

/// The audio extension that fits a codec without re-encoding.
pub fn audio_ext_for(codec: Option<&str>, fallback: &str) -> String {
    let c = codec.unwrap_or("").to_ascii_lowercase();
    if c.starts_with("opus") {
        "opus".into()
    } else if c.starts_with("mp4a") || c.starts_with("aac") {
        "m4a".into()
    } else if c.starts_with("mp3") {
        "mp3".into()
    } else if c.starts_with("vorbis") {
        "ogg".into()
    } else if c.starts_with("flac") {
        "flac".into()
    } else {
        fallback.into()
    }
}
