//! What gets downloaded: one piece of media and the streams that make it up.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StreamKind {
    /// Picture only; needs an audio stream merged in.
    Video,
    /// Sound only.
    Audio,
    /// Picture and sound in one file.
    Muxed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// A plain file over HTTP(S), fetched in parallel byte ranges when the server allows it.
    Http,
    /// An HLS playlist (`.m3u8`), fetched segment by segment.
    Hls,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stream {
    pub url: String,
    pub ext: String,
    pub kind: StreamKind,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Audio codec for audio streams, video codec otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    /// The largest range one request may ask for; some hosts throttle bigger ones.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_limit: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Media {
    pub id: String,
    pub title: String,
    pub extractor: String,
    pub webpage_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploader: Option<String>,
    pub streams: Vec<Stream>,
}

impl Media {
    /// Sum of the known stream sizes, if every stream has one.
    pub fn size(&self) -> Option<u64> {
        self.streams.iter().map(|s| s.size).sum()
    }

    pub fn has_video(&self) -> bool {
        self.streams.iter().any(|s| s.kind != StreamKind::Audio)
    }
}

/// Picks a file extension from a MIME type, for links that don't carry one.
pub fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let m = mime.split(';').next()?.trim().to_ascii_lowercase();
    Some(match m.as_str() {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/quicktime" => "mov",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" | "audio/x-m4a" => "m4a",
        "audio/ogg" | "audio/opus" => "opus",
        "audio/webm" => "webm",
        "audio/flac" | "audio/x-flac" => "flac",
        "audio/wav" | "audio/x-wav" => "wav",
        "audio/aac" => "aac",
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        _ => return None,
    })
}

const MEDIA_EXTS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "ts", "mp3", "m4a", "aac", "opus", "ogg", "oga", "flac", "wav", "jpg", "jpeg", "png", "gif",
    "webp", "pdf",
];

pub fn is_media_ext(ext: &str) -> bool {
    MEDIA_EXTS.contains(&ext.to_ascii_lowercase().as_str())
}

pub fn is_audio_ext(ext: &str) -> bool {
    matches!(
        ext.to_ascii_lowercase().as_str(),
        "mp3" | "m4a" | "aac" | "opus" | "ogg" | "oga" | "flac" | "wav"
    )
}
