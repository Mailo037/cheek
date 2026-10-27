//! HLS: picks a variant from the master playlist, fetches the segments in parallel and
//! writes them back to back into one file.

use crate::download::Shared;
use crate::http;
use crate::media::Stream;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use reqwest::Client;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt;
use url::Url;

#[derive(Debug, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub height: Option<u32>,
}

#[derive(Debug, PartialEq, Default)]
pub struct MediaPlaylist {
    pub init: Option<String>,
    pub segments: Vec<String>,
    pub encrypted: bool,
    pub byte_ranges: bool,
}

fn attr<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let i = line.find(&format!("{key}="))? + key.len() + 1;
    let rest = &line[i..];
    if let Some(stripped) = rest.strip_prefix('"') {
        stripped.split('"').next()
    } else {
        rest.split(',').next()
    }
}

fn join(base: &Url, href: &str) -> String {
    base.join(href.trim())
        .map(|u| u.to_string())
        .unwrap_or_else(|_| href.trim().to_string())
}

/// Variants of a master playlist, or `None` when this already is a media playlist.
pub fn parse_master(text: &str, base: &Url) -> Option<Vec<Variant>> {
    if !text.contains("#EXT-X-STREAM-INF") {
        return None;
    }
    let mut out = Vec::new();
    let mut pending: Option<(u64, Option<u32>)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(info) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let bandwidth = attr(info, "BANDWIDTH").and_then(|b| b.parse().ok()).unwrap_or(0);
            let height = attr(info, "RESOLUTION")
                .and_then(|r| r.split('x').nth(1))
                .and_then(|h| h.parse().ok());
            pending = Some((bandwidth, height));
        } else if !line.is_empty()
            && !line.starts_with('#')
            && let Some((bandwidth, height)) = pending.take()
        {
            out.push(Variant {
                url: join(base, line),
                bandwidth,
                height,
            });
        }
    }
    Some(out)
}

pub fn parse_media(text: &str, base: &Url) -> MediaPlaylist {
    let mut p = MediaPlaylist::default();
    for line in text.lines().map(str::trim) {
        if let Some(key) = line.strip_prefix("#EXT-X-KEY:") {
            p.encrypted |= attr(key, "METHOD").is_some_and(|m| m != "NONE");
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            p.init = attr(map, "URI").map(|u| join(base, u));
        } else if line.starts_with("#EXT-X-BYTERANGE") {
            p.byte_ranges = true;
        } else if !line.is_empty() && !line.starts_with('#') {
            p.segments.push(join(base, line));
        }
    }
    p
}

/// The best variant no taller than `max_height`.
pub fn choose(variants: &[Variant], max_height: Option<u32>) -> Option<&Variant> {
    let fits = |v: &&Variant| max_height.is_none_or(|m| v.height.is_none_or(|h| h <= m));
    variants
        .iter()
        .filter(fits)
        .max_by_key(|v| (v.height, v.bandwidth))
        .or_else(|| variants.iter().min_by_key(|v| v.bandwidth))
}

async fn text(client: &Client, stream: &Stream, url: &str) -> Result<(String, Url)> {
    let mut s = stream.clone();
    s.url = url.to_string();
    let resp = http::get(client, &s).send().await?.error_for_status()?;
    let base = resp.url().clone();
    Ok((resp.text().await?, base))
}

/// Downloads every segment of the stream into `dest` (MPEG-TS or fragmented MP4).
pub async fn fetch(shared: &Shared, stream: &Stream, dest: &Path, max_height: Option<u32>) -> Result<u64> {
    let (mut body, mut base) = text(&shared.client, stream, &stream.url).await.context("loading the playlist")?;
    if let Some(variants) = parse_master(&body, &base) {
        let v = choose(&variants, max_height).context("the playlist has no streams")?;
        (body, base) = text(&shared.client, stream, &v.url.clone())
            .await
            .context("loading the variant playlist")?;
    }
    let playlist = parse_media(&body, &base);
    if playlist.encrypted {
        bail!("this HLS stream is encrypted; use yt-dlp for it");
    }
    if playlist.byte_ranges {
        bail!("HLS byte-range playlists aren't supported yet; use yt-dlp for it");
    }
    let parts: Vec<String> = playlist.init.into_iter().chain(playlist.segments).collect();
    if parts.is_empty() {
        bail!("the playlist has no segments");
    }

    // Segments land in memory and are written in order as soon as the next one is there.
    let count = parts.len();
    let fetched = Arc::new(AtomicU64::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let progress = shared.progress.clone();
    let results = futures_util::stream::iter(parts.into_iter().enumerate().map(|(i, url)| {
        let (client, permits, stream) = (shared.client.clone(), shared.permits.clone(), stream.clone());
        let (fetched, finished, progress) = (fetched.clone(), finished.clone(), progress.clone());
        async move {
            let _p = permits.acquire().await?;
            let mut s = stream;
            s.url = url;
            let mut attempt = 0;
            let bytes = loop {
                match async { anyhow::Ok(http::get(&client, &s).send().await?.error_for_status()?.bytes().await?) }.await {
                    Ok(b) => break b,
                    Err(e) if attempt >= 4 => return Err(e.context(format!("segment {}", i + 1))),
                    Err(_) => {
                        attempt += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(400 * attempt)).await;
                    }
                }
            };
            let total = fetched.fetch_add(bytes.len() as u64, Ordering::Relaxed) + bytes.len() as u64;
            let n = finished.fetch_add(1, Ordering::Relaxed) as u64 + 1;
            // The real size is only known at the end; extrapolate from what came so far.
            progress.set_total_at_least(total * count as u64 / n);
            progress.add(bytes.len() as u64);
            anyhow::Ok(bytes)
        }
    }))
    .buffered(shared.options.connections.max(1) * 2);

    let mut out = tokio::fs::File::create(dest).await?;
    let mut written = 0u64;
    let mut results = std::pin::pin!(results);
    while let Some(piece) = results.next().await {
        let piece = piece?;
        out.write_all(&piece).await?;
        written += piece.len() as u64;
    }
    out.flush().await?;
    progress.set_total_at_least(written);
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn master_playlist() {
        let base = Url::parse("https://cdn.example.com/v/master.m3u8").unwrap();
        let text = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\nhttps://other.example.com/hi.m3u8\n";
        let v = parse_master(text, &base).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].url, "https://cdn.example.com/v/low/index.m3u8");
        assert_eq!(v[1].height, Some(1080));
        assert_eq!(choose(&v, Some(720)).unwrap().height, Some(360));
        assert_eq!(choose(&v, None).unwrap().height, Some(1080));
    }

    #[test]
    fn media_playlist() {
        let base = Url::parse("https://cdn.example.com/v/low/index.m3u8").unwrap();
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg1.m4s\n#EXTINF:4.0,\nseg2.m4s\n#EXT-X-ENDLIST\n";
        let p = parse_media(text, &base);
        assert_eq!(p.init.as_deref(), Some("https://cdn.example.com/v/low/init.mp4"));
        assert_eq!(p.segments.len(), 2);
        assert!(!p.encrypted);
        assert!(parse_media("#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\nseg.ts", &base).encrypted);
    }
}
