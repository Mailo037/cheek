//! ffmpeg for the last step: joining video and audio, pulling out the sound, remuxing HLS,
//! and writing the tags and the cover into the file. Streams are copied whenever the
//! container allows it; re-encoding only happens for an audio format the source doesn't have.

use crate::extract::which;
use crate::media::Media;
use anyhow::{Context, Result, bail};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;

#[derive(Clone)]
pub struct Ffmpeg {
    bin: PathBuf,
}

/// A picture for the file's cover, in a format the containers take (JPEG or PNG).
pub struct Cover {
    pub path: PathBuf,
    pub mime: &'static str,
}

impl Ffmpeg {
    pub fn find(explicit: Option<&PathBuf>) -> Option<Ffmpeg> {
        explicit.cloned().or_else(|| which("ffmpeg")).map(|bin| Ffmpeg { bin })
    }

    async fn run(&self, args: &[OsString]) -> Result<()> {
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
    pub async fn mux(&self, inputs: &[PathBuf], out: &Path, media: &Media, from_hls: bool, cover: Option<&Cover>) -> Result<()> {
        let ext = ext_of(out);
        let art = Art::new(cover, &ext, inputs.len(), out).await?;
        let mut args: Vec<OsString> = Vec::new();
        for i in inputs {
            args.extend(["-i".into(), i.into()]);
        }
        args.extend(art.inputs());
        if inputs.len() > 1 {
            args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                format!("{}:a:0", inputs.len() - 1).into(),
            ]);
        } else if art.is_some() {
            // Spelled out, or ffmpeg's own pick could take the cover for the video.
            args.extend(["-map".into(), "0:v:0?".into(), "-map".into(), "0:a:0?".into()]);
        }
        args.extend(["-c".into(), "copy".into()]);
        args.extend(art.outputs(usize::from(inputs.len() > 1 || media.has_video())));
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
        let result = self.run(&args).await;
        art.clean().await;
        result
    }

    /// Writes the audio of `input` as `out`, copying when the codec already fits.
    pub async fn audio(&self, input: &Path, codec: Option<&str>, out: &Path, media: &Media, cover: Option<&Cover>) -> Result<()> {
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
        let art = Art::new(cover, &ext, 1, out).await?;
        let mut args: Vec<OsString> = vec!["-i".into(), input.into()];
        args.extend(art.inputs());
        if art.is_some() {
            args.extend(["-map".into(), "0:a:0".into()]);
        } else {
            args.push("-vn".into());
        }
        args.extend(encode.iter().map(Into::into));
        args.extend(art.outputs(0));
        args.extend(metadata(media));
        args.push(out.into());
        let result = self.run(&args).await;
        art.clean().await;
        result
    }

    /// Copies `input` as it is into `out`, adding the tags and the cover: for files that
    /// needed no other ffmpeg step. `video` says whether `input` has a picture stream.
    pub async fn tag(&self, input: &Path, out: &Path, media: &Media, video: bool, cover: Option<&Cover>) -> Result<()> {
        let ext = ext_of(out);
        let art = Art::new(cover, &ext, 1, out).await?;
        let mut args: Vec<OsString> = vec!["-i".into(), input.into()];
        args.extend(art.inputs());
        args.extend([
            "-map".into(),
            "0".into(),
            "-dn".into(),
            "-ignore_unknown".into(),
            "-c".into(),
            "copy".into(),
        ]);
        args.extend(art.outputs(usize::from(video)));
        if ext == "mp4" || ext == "m4a" || ext == "mov" {
            args.extend(["-movflags".into(), "+faststart".into()]);
        }
        args.extend(metadata(media));
        args.push(out.into());
        let result = self.run(&args).await;
        art.clean().await;
        result
    }

    /// The thumbnail at `src` as a picture every container takes: JPEG and PNG stay as they
    /// are, anything else (YouTube's WebP, for one) becomes a JPEG next to it.
    pub async fn cover(&self, src: &Path) -> Result<Cover> {
        let head: Vec<u8> = tokio::fs::read(src).await?.into_iter().take(8).collect();
        if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Ok(Cover {
                path: src.to_path_buf(),
                mime: "image/jpeg",
            });
        }
        if head.starts_with(&[0x89, b'P', b'N', b'G']) {
            return Ok(Cover {
                path: src.to_path_buf(),
                mime: "image/png",
            });
        }
        let jpg = src.with_extension("jpg");
        self.run(&[
            "-i".into(),
            src.into(),
            "-frames:v".into(),
            "1".into(),
            "-q:v".into(),
            "2".into(),
            jpg.clone().into(),
        ])
        .await?;
        Ok(Cover {
            path: jpg,
            mime: "image/jpeg",
        })
    }
}

/// How one container carries a cover.
enum Art<'a> {
    None,
    /// MP4, M4A, MOV, MP3 and FLAC: a picture stream marked as the attached picture.
    Stream {
        cover: &'a Cover,
        input: usize,
        mp3: bool,
    },
    /// Matroska: a file attachment.
    Attachment(&'a Cover),
    /// Ogg and Opus: a FLAC picture block in the comments. It is too big for a command line,
    /// so it comes in as the global tags of an ffmetadata file.
    Comment {
        file: PathBuf,
        input: usize,
    },
}

impl<'a> Art<'a> {
    async fn new(cover: Option<&'a Cover>, ext: &str, inputs: usize, out: &Path) -> Result<Art<'a>> {
        let Some(cover) = cover else { return Ok(Art::None) };
        Ok(match ext {
            "mp4" | "m4a" | "mov" | "mp3" | "flac" => Art::Stream {
                cover,
                input: inputs,
                mp3: ext == "mp3",
            },
            "mkv" | "mka" => Art::Attachment(cover),
            "opus" | "ogg" | "oga" => {
                let block = picture_block(&tokio::fs::read(&cover.path).await?, cover.mime);
                let file = out.with_extension("ffmeta");
                let text = format!(";FFMETADATA1\nMETADATA_BLOCK_PICTURE={}\n", escape(&base64(&block)));
                tokio::fs::write(&file, text).await?;
                Art::Comment { file, input: inputs }
            }
            _ => Art::None,
        })
    }

    fn is_some(&self) -> bool {
        !matches!(self, Art::None)
    }

    fn inputs(&self) -> Vec<OsString> {
        match self {
            Art::Stream { cover, .. } => vec!["-i".into(), cover.path.clone().into()],
            Art::Comment { file, .. } => vec!["-f".into(), "ffmetadata".into(), "-i".into(), file.into()],
            _ => Vec::new(),
        }
    }

    /// `videos` is how many picture streams come before the cover in the output.
    fn outputs(&self, videos: usize) -> Vec<OsString> {
        match self {
            Art::None => Vec::new(),
            Art::Stream { input, mp3, .. } => {
                let mut v: Vec<OsString> = vec![
                    "-map".into(),
                    format!("{input}:v:0").into(),
                    format!("-c:v:{videos}").into(),
                    "copy".into(),
                    format!("-disposition:v:{videos}").into(),
                    "attached_pic".into(),
                ];
                if *mp3 {
                    v.extend([
                        "-id3v2_version".into(),
                        "3".into(),
                        "-metadata:s:v".into(),
                        "title=Album cover".into(),
                        "-metadata:s:v".into(),
                        "comment=Cover (front)".into(),
                    ]);
                }
                v
            }
            Art::Attachment(cover) => vec![
                "-attach".into(),
                cover.path.clone().into(),
                "-metadata:s:t".into(),
                format!("mimetype={}", cover.mime).into(),
                "-metadata:s:t".into(),
                format!("filename=cover.{}", if cover.mime == "image/png" { "png" } else { "jpg" }).into(),
            ],
            // Into the sound's own comments as well as the file's, whichever a player reads.
            Art::Comment { input, .. } => vec![
                "-map_metadata".into(),
                format!("{input}:g").into(),
                "-map_metadata:s:a:0".into(),
                format!("{input}:g").into(),
            ],
        }
    }

    async fn clean(&self) {
        if let Art::Comment { file, .. } = self {
            tokio::fs::remove_file(file).await.ok();
        }
    }
}

/// A FLAC METADATA_BLOCK_PICTURE for the front cover, the way Vorbis comments carry it.
fn picture_block(data: &[u8], mime: &str) -> Vec<u8> {
    let mut b = Vec::with_capacity(data.len() + 64);
    let put = |b: &mut Vec<u8>, n: usize| b.extend_from_slice(&(n as u32).to_be_bytes());
    put(&mut b, 3); // front cover
    put(&mut b, mime.len());
    b.extend_from_slice(mime.as_bytes());
    put(&mut b, 0); // no description
    for _ in 0..4 {
        put(&mut b, 0); // width, height, depth and colours may stay unknown
    }
    put(&mut b, data.len());
    b.extend_from_slice(data);
    b
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let byte = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (byte(0) << 16) | (byte(1) << 8) | byte(2);
        for i in 0..4 {
            s.push(if i <= chunk.len() {
                ABC[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    s
}

/// ffmetadata files escape `=`, `;`, `#`, `\` and line breaks with a backslash.
fn escape(value: &str) -> String {
    let mut s = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '=' | ';' | '#' | '\\' | '\n') {
            s.push('\\');
        }
        s.push(c);
    }
    s
}

fn metadata(media: &Media) -> Vec<OsString> {
    let mut v: Vec<OsString> = vec!["-metadata".into(), format!("title={}", media.title).into()];
    if let Some(u) = &media.uploader {
        v.extend(["-metadata".into(), format!("artist={u}").into()]);
    }
    if !media.webpage_url.is_empty() {
        v.extend(["-metadata".into(), format!("comment={}", media.webpage_url).into()]);
    }
    v
}

fn ext_of(p: &Path) -> String {
    p.extension().and_then(OsStr::to_str).unwrap_or("").to_ascii_lowercase()
}

/// Whether ffmpeg can write tags into this kind of file without re-encoding.
pub fn taggable(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp4" | "m4a" | "mov" | "mkv" | "mka" | "webm" | "mp3" | "opus" | "ogg" | "oga" | "flac"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_pads_like_the_standard() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xFB, 0xFF]), "+/8=");
    }

    #[test]
    fn picture_block_layout() {
        let b = picture_block(&[1, 2, 3], "image/jpeg");
        assert_eq!(&b[0..4], &3u32.to_be_bytes());
        assert_eq!(&b[4..8], &10u32.to_be_bytes());
        assert_eq!(&b[8..18], b"image/jpeg");
        assert_eq!(&b[b.len() - 7..b.len() - 3], &3u32.to_be_bytes());
        assert_eq!(&b[b.len() - 3..], &[1, 2, 3]);
        assert_eq!(b.len(), 4 + 4 + 10 + 4 + 16 + 4 + 3);
    }

    #[test]
    fn ffmetadata_escapes() {
        assert_eq!(escape("a=b;c#d\\e"), "a\\=b\\;c\\#d\\\\e");
    }
}
