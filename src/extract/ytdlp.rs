//! yt-dlp as the resolver: it knows the 1,800 sites and keeps up with them, cheek does the
//! downloading. Reads the JSON that `yt-dlp -J` prints (from a file, stdin or by running it).

use crate::media::{Media, Protocol, Stream, StreamKind};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

/// Runs `yt-dlp -J` for `url` with a yt-dlp format selector and reads its answer.
pub async fn resolve(bin: &Path, url: &str, format: &str) -> Result<Media> {
    let out = tokio::process::Command::new(bin)
        .args(["-J", "--no-playlist", "--no-warnings", "-f", format, url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("running {}", bin.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "yt-dlp failed: {}",
            err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no output")
        );
    }
    from_info_json(&serde_json::from_slice(&out.stdout).context("yt-dlp printed something that isn't JSON")?)
}

/// Turns yt-dlp's info dict into a `Media`. With a merged format selection the streams
/// come from `requested_formats`; otherwise the top level is the one format.
pub fn from_info_json(v: &Value) -> Result<Media> {
    let s = |o: &Value, k: &str| o.get(k).and_then(Value::as_str).map(str::to_string);
    let formats: Vec<&Value> = match v.get("requested_formats").and_then(Value::as_array) {
        Some(list) if !list.is_empty() => list.iter().collect(),
        _ => vec![v],
    };
    let streams = formats
        .into_iter()
        .map(|format| {
            let mut format = format.clone();
            for key in ["http_headers", "is_live", "has_drm", "extra_param_to_segment_url"] {
                if format.get(key).is_none()
                    && let Some(value) = v.get(key)
                {
                    format[key] = value.clone();
                }
            }
            stream(&format)
        })
        .collect::<Result<Vec<_>>>()?;
    if streams.is_empty() {
        bail!("yt-dlp picked no format");
    }
    Ok(Media {
        id: s(v, "id").unwrap_or_else(|| "NA".into()),
        title: s(v, "title").or_else(|| s(v, "fulltitle")).unwrap_or_else(|| "NA".into()),
        extractor: s(v, "extractor_key")
            .or_else(|| s(v, "extractor"))
            .unwrap_or_else(|| "yt-dlp".into()),
        webpage_url: s(v, "webpage_url").or_else(|| s(v, "original_url")).unwrap_or_default(),
        thumbnail: s(v, "thumbnail"),
        duration: v.get("duration").and_then(Value::as_f64),
        uploader: s(v, "uploader").or_else(|| s(v, "channel")),
        streams,
    })
}

fn stream(f: &Value) -> Result<Stream> {
    let s = |k: &str| f.get(k).and_then(Value::as_str).map(str::to_string);
    if f.get("has_drm").and_then(Value::as_bool) == Some(true) {
        bail!("DRM-protected formats aren't supported; use yt-dlp for it");
    }
    let url = s("url").or_else(|| s("fragment_base_url")).context("a format has no URL")?;
    let protocol = match s("protocol").as_deref().unwrap_or("https") {
        "http" | "https" => Protocol::Http,
        "m3u8" | "m3u8_native" => Protocol::Hls,
        "http_dash_segments" | "dash_frag_urls" => Protocol::Fragments,
        other => bail!("protocol {other} isn't supported yet; download this one with yt-dlp"),
    };
    let fragments = if protocol == Protocol::Fragments {
        if f.get("is_live").and_then(Value::as_bool) == Some(true) {
            bail!("live DASH isn't supported; use yt-dlp for it");
        }
        let parts = f
            .get("fragments")
            .and_then(Value::as_array)
            .filter(|p| !p.is_empty())
            .context("fragmented format has no static fragment list; use yt-dlp to resolve it again")?;
        let base = url::Url::parse(f.get("fragment_base_url").and_then(Value::as_str).unwrap_or(&url))?;
        parts
            .iter()
            .map(|part| {
                if part.get("range").is_some() || part.get("byte_range").is_some() || part.get("decrypt_info").is_some() {
                    bail!("ranged or encrypted DASH fragments aren't supported; use yt-dlp for it");
                }
                let href = part
                    .get("url")
                    .or_else(|| part.get("path"))
                    .and_then(Value::as_str)
                    .context("a fragment has no URL or path")?;
                let mut fragment = base.join(href)?;
                if !matches!(fragment.scheme(), "http" | "https") {
                    bail!("fragment URLs must use HTTP(S)");
                }
                if let Some(query) = f.get("extra_param_to_segment_url").and_then(Value::as_str) {
                    let extra: Vec<_> = url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
                    let keys: Vec<_> = extra.iter().map(|(k, _)| k.as_str()).collect();
                    let mut pairs: Vec<_> = fragment
                        .query_pairs()
                        .into_owned()
                        .filter(|(k, _)| !keys.contains(&k.as_str()))
                        .collect();
                    pairs.extend(extra);
                    fragment.set_query(None);
                    fragment.query_pairs_mut().extend_pairs(pairs);
                }
                Ok(fragment.to_string())
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let none = |k: &str| s(k).is_some_and(|c| c == "none");
    let kind = match (none("vcodec"), none("acodec")) {
        (true, _) => StreamKind::Audio,
        (false, true) => StreamKind::Video,
        _ => StreamKind::Muxed,
    };
    let headers: BTreeMap<String, String> = f
        .get("http_headers")
        .and_then(Value::as_object)
        .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
        .unwrap_or_default();
    Ok(Stream {
        ext: s("ext").unwrap_or_else(|| "mp4".into()),
        codec: if kind == StreamKind::Audio { s("acodec") } else { s("vcodec") },
        height: f.get("height").and_then(Value::as_u64).map(|h| h as u32),
        size: f
            .get("filesize")
            .and_then(Value::as_u64)
            .or_else(|| f.get("filesize_approx").and_then(Value::as_u64)),
        format_id: s("format_id"),
        chunk_limit: f.pointer("/downloader_options/http_chunk_size").and_then(Value::as_u64),
        url,
        kind,
        protocol,
        headers,
        fragments,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn merged_selection() {
        let v = json!({
            "id": "SkVqJ1SGeL0", "title": "Caminandes 3: Llamigos", "extractor_key": "Youtube",
            "webpage_url": "https://www.youtube.com/watch?v=SkVqJ1SGeL0", "duration": 150.0,
            "requested_formats": [
                {"format_id": "137", "url": "https://v/1", "ext": "mp4", "vcodec": "avc1.640028", "acodec": "none", "height": 1080,
                 "protocol": "https", "filesize": 22000000, "http_headers": {"User-Agent": "UA"},
                 "downloader_options": {"http_chunk_size": 10485760}},
                {"format_id": "251", "url": "https://v/2", "ext": "webm", "vcodec": "none", "acodec": "opus", "protocol": "https"}
            ]
        });
        let m = from_info_json(&v).unwrap();
        assert_eq!(m.streams.len(), 2);
        assert_eq!(m.streams[0].kind, StreamKind::Video);
        assert_eq!(m.streams[0].chunk_limit, Some(10485760));
        assert_eq!(m.streams[1].kind, StreamKind::Audio);
        assert_eq!(m.streams[1].codec.as_deref(), Some("opus"));
        assert_eq!(m.streams[0].headers["User-Agent"], "UA");
    }

    #[test]
    fn single_format() {
        let v = json!({"id": "x", "title": "t", "url": "https://v/x.m3u8", "ext": "mp4", "protocol": "m3u8_native", "vcodec": "avc1", "acodec": "mp4a"});
        let m = from_info_json(&v).unwrap();
        assert_eq!(m.streams[0].protocol, Protocol::Hls);
        assert_eq!(m.streams[0].kind, StreamKind::Muxed);
    }

    #[test]
    fn dash_fragments_and_inherited_metadata() {
        let m = from_info_json(&json!({
            "http_headers": {"Referer": "https://example.com/watch"},
            "extra_param_to_segment_url": "token=new",
            "requested_formats": [{
                "url": "https://cdn.example.com/manifest.mpd",
                "fragment_base_url": "https://cdn.example.com/audio/",
                "protocol": "http_dash_segments", "vcodec": "none", "acodec": "aac", "ext": "m4a",
                "fragments": [{"path": "init.mp4"}, {"url": "seg.m4s?token=old&x=1"}]
            }]
        }))
        .unwrap();
        let s = &m.streams[0];
        assert_eq!(s.protocol, Protocol::Fragments);
        assert_eq!(s.kind, StreamKind::Audio);
        assert_eq!(s.headers["Referer"], "https://example.com/watch");
        assert_eq!(
            s.fragments,
            [
                "https://cdn.example.com/audio/init.mp4?token=new",
                "https://cdn.example.com/audio/seg.m4s?x=1&token=new"
            ]
        );
        let roundtrip: Media = serde_json::from_value(serde_json::to_value(&m).unwrap()).unwrap();
        assert_eq!(roundtrip.streams[0].fragments, s.fragments);
    }

    #[test]
    fn rejects_incomplete_live_and_encrypted_dash() {
        let mut v = json!({"url": "https://v/manifest.mpd", "protocol": "http_dash_segments"});
        assert!(from_info_json(&v).is_err());
        v["fragments"] = json!([{"path": "init.mp4"}]);
        v["is_live"] = json!(true);
        assert!(from_info_json(&v).is_err());
        v["is_live"] = json!(false);
        v["has_drm"] = json!(true);
        assert!(from_info_json(&v).is_err());
        v["has_drm"] = json!(false);
        v["fragments"] = json!([{"path": "init.mp4", "range": {"start": 0, "end": 100}}]);
        assert!(from_info_json(&v).is_err());
    }
}
