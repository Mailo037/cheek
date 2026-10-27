//! Links that need no site knowledge: a direct file, an HLS playlist, or a page that names
//! its media in `og:video`, `twitter:player:stream`, `<video>`/`<source>` or JSON-LD.

use crate::media::{Media, Protocol, Stream, StreamKind, ext_for_mime, is_audio_ext, is_media_ext};
use anyhow::{Context, Result, anyhow};
use regex::Regex;
use reqwest::{Client, Response, header};
use std::sync::LazyLock;
use url::Url;

/// The link is a page cheek can't find media on by itself.
#[derive(Debug)]
pub struct Unsupported;

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no media found on this page")
    }
}

impl std::error::Error for Unsupported {}

pub async fn extract(client: &Client, url: &str) -> Result<Media> {
    let resp = client
        .get(url)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .context("connecting")?
        .error_for_status()?;
    let final_url = resp.url().clone();
    let mime = content_type(&resp);

    if let Some(stream) = media_stream(&final_url, &mime, &resp) {
        return Ok(single(&final_url, url, stream));
    }
    if !mime.contains("html") {
        return Err(anyhow!(Unsupported));
    }
    // The ranged probe returned at most one byte of the page; fetch it whole.
    let page = client.get(final_url.as_str()).send().await?.error_for_status()?;
    let page_url = page.url().clone();
    let html = page.text().await?;
    let found = Page::parse(&html, &page_url);

    for candidate in &found.media {
        let Ok(u) = Url::parse(candidate) else { continue };
        let Ok(r) = client.get(u.as_str()).header(header::RANGE, "bytes=0-0").send().await else {
            continue;
        };
        if !r.status().is_success() {
            continue;
        }
        let m = content_type(&r);
        if let Some(stream) = media_stream(r.url(), &m, &r) {
            let mut media = single(r.url(), url, stream);
            if let Some(t) = &found.title {
                media.title = t.clone();
            }
            media.thumbnail = found.thumbnail.clone();
            media.webpage_url = page_url.to_string();
            return Ok(media);
        }
    }
    Err(anyhow!(Unsupported))
}

fn content_type(r: &Response) -> String {
    r.headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn total_size(r: &Response) -> Option<u64> {
    if let Some(range) = r.headers().get(header::CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
        return range.rsplit('/').next()?.parse().ok();
    }
    r.content_length().filter(|n| *n > 1)
}

fn path_ext(u: &Url) -> Option<String> {
    let last = u.path_segments()?.next_back()?;
    let (_, ext) = last.rsplit_once('.')?;
    Some(ext.to_ascii_lowercase())
}

/// A stream when this response is media (by MIME type, or by extension for servers that
/// call everything `application/octet-stream`).
fn media_stream(u: &Url, mime: &str, r: &Response) -> Option<Stream> {
    let ext = path_ext(u);
    if mime.contains("mpegurl") || ext.as_deref() == Some("m3u8") {
        return Some(Stream {
            url: u.to_string(),
            ext: "mp4".into(),
            kind: StreamKind::Muxed,
            protocol: Protocol::Hls,
            format_id: Some("hls".into()),
            size: None,
            codec: None,
            height: None,
            headers: Default::default(),
            chunk_limit: None,
        });
    }
    let by_mime = ext_for_mime(mime);
    let is_media = mime.starts_with("video/") || mime.starts_with("audio/") || mime.starts_with("image/") || by_mime.is_some();
    let ext = match (by_mime, ext) {
        (_, Some(e)) if is_media_ext(&e) => e,
        (Some(e), _) => e.to_string(),
        _ => return None,
    };
    if !is_media && !mime.contains("octet-stream") && !mime.is_empty() {
        return None;
    }
    Some(Stream {
        url: u.to_string(),
        kind: if is_audio_ext(&ext) || mime.starts_with("audio/") {
            StreamKind::Audio
        } else {
            StreamKind::Muxed
        },
        ext,
        protocol: Protocol::Http,
        format_id: None,
        size: total_size(r),
        codec: None,
        height: None,
        headers: Default::default(),
        chunk_limit: None,
    })
}

fn single(final_url: &Url, original: &str, stream: Stream) -> Media {
    let name = final_url
        .path_segments()
        .and_then(|mut s| s.next_back())
        .map(percent_decode)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| final_url.host_str().unwrap_or("download").to_string());
    let title = name.rsplit_once('.').map(|(t, _)| t.to_string()).unwrap_or(name);
    Media {
        id: short_hash(final_url.as_str()),
        title,
        extractor: "Generic".into(),
        webpage_url: original.to_string(),
        thumbnail: None,
        duration: None,
        uploader: None,
        streams: vec![stream],
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Some(b) = std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Stable 11-character id for links without one (FNV-1a, base 36).
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    let mut out = String::new();
    for _ in 0..11 {
        out.push(char::from_digit((h % 36) as u32, 36).unwrap());
        h /= 36;
    }
    out
}

/// What an HTML page says about its media, best candidates first.
#[derive(Debug, Default)]
pub struct Page {
    pub title: Option<String>,
    pub thumbnail: Option<String>,
    pub media: Vec<String>,
}

static META: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<meta\s[^>]*>"#).unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)([a-z:_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap());
static TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
static TAG_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?is)<(?:video|audio|source)\s[^>]*?\bsrc\s*=\s*["']([^"']+)["']"#).unwrap());
static LD_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""contentUrl"\s*:\s*"([^"]+)""#).unwrap());

impl Page {
    pub fn parse(html: &str, base: &Url) -> Page {
        let mut page = Page::default();
        let mut by_rank: Vec<(u8, String)> = Vec::new();
        for tag in META.find_iter(html) {
            let mut key = None;
            let mut content = None;
            for a in ATTR.captures_iter(tag.as_str()) {
                let value = a.get(2).or(a.get(3)).map(|m| unescape(m.as_str()));
                match a[1].to_ascii_lowercase().as_str() {
                    "property" | "name" | "itemprop" => key = value.map(|v| v.to_ascii_lowercase()),
                    "content" => content = value,
                    _ => {}
                }
            }
            let (Some(key), Some(content)) = (key, content) else { continue };
            let rank = match key.as_str() {
                "og:video:secure_url" => 0,
                "og:video:url" | "og:video" => 1,
                "twitter:player:stream" => 2,
                "og:audio:secure_url" | "og:audio" => 3,
                "contenturl" => 4,
                "og:title" | "twitter:title" => {
                    page.title.get_or_insert(content);
                    continue;
                }
                "og:image" | "og:image:secure_url" | "twitter:image" => {
                    page.thumbnail.get_or_insert_with(|| join(base, &content));
                    continue;
                }
                _ => continue,
            };
            by_rank.push((rank, join(base, &content)));
        }
        for m in TAG_SRC.captures_iter(html) {
            by_rank.push((5, join(base, &unescape(&m[1]))));
        }
        for m in LD_URL.captures_iter(html) {
            by_rank.push((6, join(base, &m[1].replace("\\/", "/"))));
        }
        by_rank.sort_by_key(|(r, _)| *r);
        for (_, u) in by_rank {
            if !u.starts_with("blob:") && !page.media.contains(&u) {
                page.media.push(u);
            }
        }
        if page.title.is_none() {
            page.title = TITLE.captures(html).map(|c| unescape(c[1].trim())).filter(|t| !t.is_empty());
        }
        page
    }
}

fn join(base: &Url, href: &str) -> String {
    base.join(href.trim()).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string())
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_media_in_meta_and_tags() {
        let base = Url::parse("https://example.com/watch/42").unwrap();
        let html = r#"<html><head><title>Fallback</title>
            <meta property="og:title" content="A &amp; B">
            <meta content="/thumbs/42.jpg" property="og:image">
            <meta property="og:video" content="https://cdn.example.com/42.mp4">
            </head><body><video controls src="/media/42-low.webm"></video>
            <script type="application/ld+json">{"contentUrl":"https:\/\/cdn.example.com\/42-ld.mp4"}</script></body></html>"#;
        let p = Page::parse(html, &base);
        assert_eq!(p.title.as_deref(), Some("A & B"));
        assert_eq!(p.thumbnail.as_deref(), Some("https://example.com/thumbs/42.jpg"));
        assert_eq!(
            p.media,
            vec![
                "https://cdn.example.com/42.mp4",
                "https://example.com/media/42-low.webm",
                "https://cdn.example.com/42-ld.mp4"
            ]
        );
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(percent_decode("Big%20Buck%20Bunny.mp4"), "Big Buck Bunny.mp4");
        assert_eq!(short_hash("x").len(), 11);
    }
}
