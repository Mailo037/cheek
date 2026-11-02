//! HLS: picks a variant from the master playlist, fetches the segments in parallel and
//! writes them back to back into one file. Segments encrypted with AES-128 (a key the
//! playlist hands to every player; not DRM) are decrypted on the way, and byte-range
//! playlists fetch their pieces of one file.

use crate::download::Shared;
use crate::http;
use crate::media::Stream;
use aes::Aes128;
use anyhow::{Context, Result, bail, ensure};
use cbc::cipher::{BlockDecryptMut, KeyIvInit, block_padding::Pkcs7};
use futures_util::StreamExt;
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use url::Url;

#[derive(Debug, PartialEq)]
pub struct Variant {
    pub url: String,
    pub bandwidth: u64,
    pub height: Option<u32>,
}

/// An AES-128 key from `#EXT-X-KEY`, with its IV when the playlist names one.
#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub uri: String,
    pub iv: Option<[u8; 16]>,
}

/// One piece of a stream: a URL, maybe only a byte range of it, maybe encrypted.
#[derive(Debug, Clone, PartialEq)]
pub struct Part {
    pub url: String,
    /// Offset and length.
    pub range: Option<(u64, u64)>,
    pub key: Option<Key>,
    /// The segment's media sequence number: its IV when the key names none.
    pub sequence: u64,
}

impl Part {
    pub fn plain(url: String) -> Part {
        Part {
            url,
            range: None,
            key: None,
            sequence: 0,
        }
    }
}

#[derive(Debug, PartialEq, Default)]
pub struct MediaPlaylist {
    pub init: Option<Part>,
    pub segments: Vec<Part>,
    /// A key method Cheek can't open (SAMPLE-AES and the DRM schemes).
    pub unsupported_key: Option<String>,
    /// `#EXT-X-ENDLIST` (or a VOD playlist): the list is complete. Without it the stream may be live.
    pub ended: bool,
    /// `#EXT-X-TARGETDURATION`: the longest segment, in seconds.
    pub target_duration: f64,
}

/// The attributes of a tag like `#EXT-X-KEY:METHOD=AES-128,URI="k",IV=0x…`, in order.
fn attrs(line: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else { break };
        let key = rest[..eq].trim().trim_start_matches(',').trim().to_string();
        rest = &rest[eq + 1..];
        let value = if let Some(quoted) = rest.strip_prefix('"') {
            let end = quoted.find('"').unwrap_or(quoted.len());
            let v = quoted[..end].to_string();
            rest = quoted.get(end + 1..).unwrap_or("");
            v
        } else {
            let end = rest.find(',').unwrap_or(rest.len());
            let v = rest[..end].trim().to_string();
            rest = &rest[end..];
            v
        };
        rest = rest.trim_start_matches(',');
        out.push((key, value));
    }
    out
}

fn attr(line: &str, key: &str) -> Option<String> {
    attrs(line).into_iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

fn join(base: &Url, href: &str) -> String {
    base.join(href.trim())
        .map(|u| u.to_string())
        .unwrap_or_else(|_| href.trim().to_string())
}

/// `<length>[@<offset>]`.
fn byte_range(v: &str) -> Option<(u64, Option<u64>)> {
    let v = v.trim();
    let (n, o) = match v.split_once('@') {
        Some((n, o)) => (n, Some(o)),
        None => (v, None),
    };
    let offset = match o {
        Some(o) => Some(o.trim().parse().ok()?),
        None => None,
    };
    Some((n.trim().parse().ok()?, offset))
}

/// `0x` and up to 32 hex digits, right-aligned like the spec's 128-bit integer.
fn iv(v: &str) -> Option<[u8; 16]> {
    let hex = v.trim().strip_prefix("0x").or_else(|| v.trim().strip_prefix("0X"))?;
    if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let padded = format!("{hex:0>32}");
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
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
                .and_then(|r| r.split('x').nth(1).map(str::to_string))
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
    let mut key: Option<Key> = None;
    let mut sequence = 0u64;
    let mut pending_range: Option<(u64, Option<u64>)> = None;
    // Where the last byte range ended: a range without an offset carries on from there.
    let mut last_end: Option<(String, u64)> = None;
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            sequence = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            p.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if line == "#EXT-X-ENDLIST" || line.strip_prefix("#EXT-X-PLAYLIST-TYPE:").is_some_and(|t| t.trim() == "VOD") {
            p.ended = true;
        } else if let Some(tag) = line.strip_prefix("#EXT-X-KEY:") {
            match attr(tag, "METHOD").as_deref().unwrap_or("NONE") {
                "NONE" => key = None,
                "AES-128" => {
                    key = attr(tag, "URI").map(|uri| Key {
                        uri: join(base, &uri),
                        iv: attr(tag, "IV").as_deref().and_then(iv),
                    })
                }
                other => p.unsupported_key = Some(other.to_string()),
            }
        } else if let Some(map) = line.strip_prefix("#EXT-X-MAP:") {
            let range = attr(map, "BYTERANGE")
                .as_deref()
                .and_then(byte_range)
                .map(|(len, offset)| (offset.unwrap_or(0), len));
            p.init = attr(map, "URI").map(|uri| Part {
                url: join(base, &uri),
                range,
                key: key.clone(),
                sequence,
            });
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            pending_range = byte_range(v);
        } else if !line.is_empty() && !line.starts_with('#') {
            let url = join(base, line);
            let range = pending_range.take().map(|(len, offset)| {
                let start = offset.unwrap_or_else(|| last_end.as_ref().filter(|(u, _)| *u == url).map(|(_, e)| *e).unwrap_or(0));
                (start, len)
            });
            if let Some((start, len)) = range {
                last_end = Some((url.clone(), start + len));
            }
            p.segments.push(Part {
                url,
                range,
                key: key.clone(),
                sequence,
            });
            sequence += 1;
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

fn at(stream: &Stream, url: &str) -> Stream {
    let mut s = stream.clone();
    s.url = url.to_string();
    s.fragments.clear();
    s
}

/// A playlist and the address it came from, trying again when the server hiccups.
async fn text(client: &Client, stream: &Stream, url: &str) -> Result<(String, Url)> {
    let s = at(stream, url);
    let mut attempt = 0;
    loop {
        let result = http::get(client, &s).send().await;
        let retry = match &result {
            Ok(r) if r.status().is_success() => None,
            Ok(r) if r.status().is_server_error() || matches!(r.status(), StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT) => {
                Some(http::retry_delay(Some(r.headers()), attempt + 1))
            }
            Ok(_) => None,
            Err(_) => Some(http::retry_delay(None, attempt + 1)),
        };
        match retry {
            Some(delay) if attempt < 4 => {
                attempt += 1;
                tokio::time::sleep(delay).await;
            }
            _ => {
                let resp = result?.error_for_status()?;
                let base = resp.url().clone();
                return Ok((resp.text().await?, base));
            }
        }
    }
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
    if let Some(method) = &playlist.unsupported_key {
        bail!("this HLS stream is protected with {method}; Cheek only opens AES-128, use yt-dlp for it");
    }
    if !playlist.ended {
        // A live stream grows; a finished one only forgot its end tag. One more look tells them apart.
        tokio::time::sleep(Duration::from_secs_f64(playlist.target_duration.clamp(1.0, 10.0))).await;
        let (again, again_base) = text(&shared.client, stream, base.as_str())
            .await
            .context("loading the playlist again")?;
        let later = parse_media(&again, &again_base);
        if later.segments.last().map(|s| &s.url) != playlist.segments.last().map(|s| &s.url) {
            bail!("this is a live stream; Cheek downloads finished videos, use yt-dlp to record it");
        }
    }
    let parts: Vec<Part> = playlist.init.into_iter().chain(playlist.segments).collect();
    fetch_pieces(shared, stream, dest, parts).await
}

/// How far an interrupted fragment download got: the first `written` fragments, `bytes` long.
#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct Notes {
    count: usize,
    /// The first and last fragment without their query, which often carries expiring tokens.
    key: String,
    written: usize,
    bytes: u64,
}

fn notes_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".cheek");
    PathBuf::from(p)
}

/// Names the fragment list so a later run can tell whether it is the same one.
fn list_key(parts: &[Part]) -> String {
    let bare = |p: &Part| {
        let url = p.url.split(['?', '#']).next().unwrap_or(&p.url);
        match p.range {
            Some((start, len)) => format!("{url}@{start}+{len}"),
            None => url.to_string(),
        }
    };
    format!(
        "{}|{}",
        parts.first().map(bare).unwrap_or_default(),
        parts.last().map(bare).unwrap_or_default()
    )
}

/// The fragments already on disk from an earlier run, cutting off anything past the last
/// noted one. Anything that doesn't match starts over from the first fragment.
async fn resume_point(dest: &Path, count: usize, key: &str) -> (usize, u64) {
    let Ok(bytes) = tokio::fs::read(notes_path(dest)).await else {
        return (0, 0);
    };
    let Ok(notes) = serde_json::from_slice::<Notes>(&bytes) else {
        return (0, 0);
    };
    let len = tokio::fs::metadata(dest).await.map(|m| m.len()).unwrap_or(0);
    if notes.count != count || notes.key != key || notes.written == 0 || notes.written >= count || len < notes.bytes {
        return (0, 0);
    }
    (notes.written, notes.bytes)
}

/// Keys fetched once per URI and shared by every segment that uses them.
#[derive(Default)]
struct Keys(tokio::sync::Mutex<HashMap<String, [u8; 16]>>);

impl Keys {
    async fn get(&self, client: &Client, stream: &Stream, uri: &str) -> Result<[u8; 16]> {
        let mut keys = self.0.lock().await;
        if let Some(k) = keys.get(uri) {
            return Ok(*k);
        }
        let bytes = http::get(client, &at(stream, uri))
            .send()
            .await?
            .error_for_status()
            .context("loading the stream's key")?
            .bytes()
            .await?;
        ensure!(bytes.len() == 16, "the stream's key isn't an AES-128 key");
        let mut key = [0u8; 16];
        key.copy_from_slice(&bytes);
        keys.insert(uri.to_string(), key);
        Ok(key)
    }
}

/// AES-128-CBC with PKCS#7 padding, as HLS encrypts whole segments.
pub fn decrypt(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    cbc::Decryptor::<Aes128>::new(key.into(), iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(data)
        .map_err(|_| anyhow::anyhow!("a segment didn't decrypt; the key may be wrong"))
}

/// Fetch an ordered list of fragment URLs (DASH as yt-dlp lists them) into `dest`.
pub async fn fetch_parts(shared: &Shared, stream: &Stream, dest: &Path, urls: Vec<String>) -> Result<u64> {
    fetch_pieces(shared, stream, dest, urls.into_iter().map(Part::plain).collect()).await
}

/// Fetch an ordered fragment list with bounded concurrency, shared by HLS and DASH.
/// Fragments that an earlier, interrupted run already wrote are kept and skipped.
pub async fn fetch_pieces(shared: &Shared, stream: &Stream, dest: &Path, parts: Vec<Part>) -> Result<u64> {
    if parts.is_empty() {
        bail!("the playlist has no segments");
    }

    let count = parts.len();
    let key = list_key(&parts);
    let sidecar = notes_path(dest);
    let (skip, kept) = resume_point(dest, count, &key).await;
    let progress = shared.progress.clone();
    if skip > 0 {
        progress.add(kept);
        progress.set_total_at_least(kept * count as u64 / skip as u64);
    }

    // Completed segments spill to disk, so a slow early segment cannot stall the
    // network window or accumulate entire segments in memory.
    let staging = tempfile::tempdir_in(dest.parent().unwrap_or_else(|| Path::new(".")))?;
    let fetched = Arc::new(AtomicU64::new(kept));
    let finished = Arc::new(AtomicUsize::new(skip));
    let keys = Arc::new(Keys::default());
    let mut source = stream.clone();
    source.fragments.clear();
    let results = futures_util::stream::iter(parts.into_iter().enumerate().skip(skip).map(|(i, part)| {
        let (client, permits, stream) = (shared.client.clone(), shared.permits.clone(), source.clone());
        let (fetched, finished, progress, keys) = (fetched.clone(), finished.clone(), progress.clone(), keys.clone());
        let path = staging.path().join(i.to_string());
        async move {
            let s = at(&stream, &part.url);
            let mut attempt = 0;
            let mut bytes = loop {
                let mut delay = None;
                let result: Result<u64> = async {
                    let _p = permits.acquire().await?;
                    let mut request = http::get(&client, &s);
                    if let Some((start, len)) = part.range {
                        request = request.header(header::RANGE, format!("bytes={start}-{}", start + len - 1));
                    }
                    let response = request.send().await?;
                    delay = Some(http::retry_delay(Some(response.headers()), attempt + 1));
                    let response = response.error_for_status()?;
                    // A server that ignores Range sends the whole file: keep only the asked-for piece.
                    let (mut skip, mut want) = match part.range {
                        Some((start, len)) if response.status() != StatusCode::PARTIAL_CONTENT => (start, Some(len)),
                        Some((_, len)) => (0, Some(len)),
                        None => (0, None),
                    };
                    let mut body = response.bytes_stream();
                    let mut file = BufWriter::with_capacity(256 << 10, tokio::fs::File::create(&path).await?);
                    let mut bytes = 0;
                    while let Some(piece) = body.next().await {
                        let chunk = piece?;
                        let mut piece = &chunk[..];
                        if skip > 0 {
                            let n = (piece.len() as u64).min(skip);
                            piece = &piece[n as usize..];
                            skip -= n;
                        }
                        if let Some(left) = want {
                            piece = &piece[..(piece.len() as u64).min(left) as usize];
                            want = Some(left - piece.len() as u64);
                        }
                        file.write_all(piece).await?;
                        bytes += piece.len() as u64;
                        if want == Some(0) {
                            break;
                        }
                    }
                    file.flush().await?;
                    ensure!(want.is_none_or(|w| w == 0), "the server sent less than the byte range");
                    Ok(bytes)
                }
                .await;
                match result {
                    Ok(b) => break b,
                    Err(e) => {
                        let fatal = e
                            .downcast_ref::<reqwest::Error>()
                            .and_then(|e| e.status())
                            .is_some_and(|s| s.is_client_error() && s != StatusCode::TOO_MANY_REQUESTS && s != StatusCode::REQUEST_TIMEOUT);
                        if fatal || attempt >= 4 {
                            return Err(e.context(format!("segment {}", i + 1)));
                        }
                        progress.throttled();
                        attempt += 1;
                        tokio::time::sleep(delay.unwrap_or_else(|| http::retry_delay(None, attempt))).await;
                    }
                }
            };
            if let Some(k) = &part.key {
                let key = keys.get(&client, &stream, &k.uri).await?;
                let iv = k.iv.unwrap_or_else(|| (part.sequence as u128).to_be_bytes());
                let plain = decrypt(&tokio::fs::read(&path).await?, &key, &iv).with_context(|| format!("segment {}", i + 1))?;
                tokio::fs::write(&path, &plain).await?;
                bytes = plain.len() as u64;
            }
            let total = fetched.fetch_add(bytes, Ordering::Relaxed) + bytes;
            let n = finished.fetch_add(1, Ordering::Relaxed) as u64 + 1;
            progress.set_total_at_least(total * count as u64 / n);
            progress.add(bytes);
            anyhow::Ok((i, path, bytes))
        }
    }))
    .buffer_unordered(shared.options.connections.clamp(1, 64) * 2);

    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(skip == 0)
        .open(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    // A fragment that was only half written when the last run stopped goes again.
    file.set_len(kept).await?;
    let mut out = BufWriter::with_capacity(256 << 10, file);
    out.seek(std::io::SeekFrom::Start(kept)).await?;
    let mut notes = Notes {
        count,
        key,
        written: skip,
        bytes: kept,
    };
    let mut noted = Instant::now();
    let mut results = std::pin::pin!(results);
    let mut pending = std::collections::BTreeMap::new();
    let outcome: Result<()> = async {
        while let Some(piece) = results.next().await {
            let (index, path, bytes) = piece?;
            pending.insert(index, (path, bytes));
            while let Some((path, bytes)) = pending.remove(&notes.written) {
                let mut input = tokio::fs::File::open(&path).await?;
                tokio::io::copy(&mut input, &mut out).await?;
                drop(input);
                tokio::fs::remove_file(path).await?;
                notes.written += 1;
                notes.bytes += bytes;
            }
            // The bytes must be on disk before the note that counts them; at most once a second.
            if noted.elapsed() > Duration::from_secs(1) {
                noted = Instant::now();
                out.flush().await?;
                tokio::fs::write(&sidecar, serde_json::to_vec(&notes)?).await.ok();
            }
        }
        Ok(())
    }
    .await;
    if let Err(e) = outcome {
        if out.flush().await.is_ok() && notes.written > 0 {
            tokio::fs::write(&sidecar, serde_json::to_vec(&notes)?).await.ok();
        }
        return Err(e);
    }
    out.flush().await?;
    tokio::fs::remove_file(&sidecar).await.ok();
    progress.set_total_at_least(notes.bytes);
    Ok(notes.bytes)
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
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:4.0,\nseg1.m4s\n#EXTINF:4.0,\nseg2.m4s\n#EXT-X-ENDLIST\n";
        let p = parse_media(text, &base);
        assert_eq!(
            p.init.as_ref().map(|i| i.url.as_str()),
            Some("https://cdn.example.com/v/low/init.mp4")
        );
        assert_eq!(p.segments.len(), 2);
        assert!(p.ended && p.unsupported_key.is_none() && p.target_duration == 4.0);
        assert!(!parse_media("seg.ts", &base).ended);
        assert_eq!(
            parse_media("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://k\"\nseg.ts", &base)
                .unsupported_key
                .as_deref(),
            Some("SAMPLE-AES")
        );
    }

    #[test]
    fn aes_keys_and_their_ivs() {
        let base = Url::parse("https://cdn.example.com/v/index.m3u8").unwrap();
        let text = "#EXTM3U\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-KEY:METHOD=AES-128,URI=\"keys/k1?IV=no\",IV=0x0102\n#EXTINF:4,\na.ts\n#EXT-X-KEY:METHOD=AES-128,URI=\"keys/k2\"\n#EXTINF:4,\nb.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:4,\nc.ts\n";
        let p = parse_media(text, &base);
        let a = p.segments[0].key.as_ref().unwrap();
        assert_eq!(a.uri, "https://cdn.example.com/v/keys/k1?IV=no");
        let mut want = [0u8; 16];
        want[14] = 1;
        want[15] = 2;
        assert_eq!(a.iv, Some(want));
        assert_eq!(p.segments[1].key.as_ref().unwrap().iv, None);
        assert_eq!(p.segments[1].sequence, 8);
        assert!(p.segments[2].key.is_none());
    }

    #[test]
    fn byte_ranges_carry_on_from_the_last_one() {
        let base = Url::parse("https://cdn.example.com/v/index.m3u8").unwrap();
        let text = "#EXTM3U\n#EXT-X-MAP:URI=\"all.mp4\",BYTERANGE=\"800@0\"\n#EXTINF:4,\n#EXT-X-BYTERANGE:1000@800\nall.mp4\n#EXTINF:4,\n#EXT-X-BYTERANGE:500\nall.mp4\n#EXT-X-ENDLIST\n";
        let p = parse_media(text, &base);
        assert_eq!(p.init.unwrap().range, Some((0, 800)));
        assert_eq!(
            p.segments.iter().map(|s| s.range).collect::<Vec<_>>(),
            [Some((800, 1000)), Some((1800, 500))]
        );
    }

    #[test]
    fn decrypts_aes_128_cbc() {
        use cbc::cipher::BlockEncryptMut;
        let (key, iv) = ([7u8; 16], [9u8; 16]);
        let plain = b"segment bytes that aren't a multiple of sixteen".to_vec();
        let sealed = cbc::Encryptor::<Aes128>::new(&key.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&plain);
        assert_eq!(decrypt(&sealed, &key, &iv).unwrap(), plain);
        assert!(decrypt(&sealed, &[8u8; 16], &iv).is_err() || decrypt(&sealed, &[8u8; 16], &iv).unwrap() != plain);
    }
}
