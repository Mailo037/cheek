//! Public Glyph shares, using the same source endpoint as Glyph's web player.

use super::generic;
use crate::media::Media;
use anyhow::{Context, Result, ensure};
use regex::Regex;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::LazyLock;
use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    id: String,
    index: Option<usize>,
}

impl Target {
    pub fn parse(input: &str) -> Result<Option<Self>> {
        let Ok(url) = Url::parse(input) else { return Ok(None) };
        if !matches!(url.host_str(), Some("glyph.my" | "www.glyph.my")) {
            return Ok(None);
        }
        ensure!(matches!(url.scheme(), "http" | "https"), "Glyph links require HTTP or HTTPS");
        let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
        let (id, index) = match parts.as_slice() {
            ["p", id] | ["embed", "p", id] => (*id, None),
            ["p", id, "listen", n] => (*id, Some(n.parse::<usize>().context("Glyph song index must be non-negative")?)),
            // Covers and direct API audio links keep using the generic extractor.
            _ => return Ok(None),
        };
        ensure!(
            matches!(id.len(), 8 | 12) && id.bytes().all(|b| b.is_ascii_alphanumeric()),
            "invalid Glyph share ID"
        );
        Ok(Some(Self { id: id.into(), index }))
    }

    fn page(&self) -> String {
        format!("https://glyph.my/p/{}", self.id)
    }
}

#[derive(Debug)]
struct Song {
    index: usize,
    title: String,
    artist: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Playlist {
    #[serde(rename = "_type")]
    pub kind: String,
    pub id: String,
    pub title: String,
    pub extractor: String,
    pub webpage_url: String,
    pub thumbnail: String,
    pub entries: Vec<PlaylistEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub id: String,
    pub title: String,
    pub uploader: String,
    pub url: String,
    pub thumbnail: String,
    pub extractor_key: String,
}

static JSON_LD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<script\s+type="application/ld\+json"\s*>(.*?)</script>"#).unwrap());

fn playlist(html: &str, page: &str, id: &str) -> Result<Option<Playlist>> {
    let songs = songs(html);
    let metadata = JSON_LD.captures(html).and_then(|c| serde_json::from_str::<Value>(&c[1]).ok());
    let count = metadata
        .as_ref()
        .and_then(|m| m["numTracks"].as_u64())
        .unwrap_or(songs.len() as u64);
    ensure!(
        count <= songs.len() as u64,
        "Glyph's web player exposes only {} of {count} songs; refusing to truncate this playlist",
        songs.len()
    );
    if songs.len() <= 1 {
        return Ok(None);
    }
    let title = metadata
        .as_ref()
        .and_then(|m| m["name"].as_str())
        .map(str::to_owned)
        .or_else(|| generic::Page::parse(html, &Url::parse(page).ok()?).title)
        .unwrap_or_else(|| id.into());
    Ok(Some(Playlist {
        kind: "playlist".into(),
        id: id.into(),
        title,
        extractor: "Glyph".into(),
        webpage_url: page.into(),
        thumbnail: format!("{page}/cover"),
        entries: songs
            .into_iter()
            .map(|song| PlaylistEntry {
                id: format!("{id}-{}", song.index),
                title: song.title,
                uploader: song.artist,
                url: format!("{page}/listen/{}", song.index),
                thumbnail: format!("{page}/cover/{}", song.index),
                extractor_key: "Glyph".into(),
            })
            .collect(),
    }))
}

pub async fn resolve(client: &Client, target: &Target) -> Result<super::Resolved> {
    let page = target.page();
    let html = client.get(&page).send().await?.error_for_status()?.text().await?;
    if target.index.is_none()
        && let Some(list) = playlist(&html, &page, &target.id)?
    {
        return Ok(super::Resolved::Playlist(list));
    }
    extract_song(client, &page, select_song(&html, target.index)?)
        .await
        .map(super::Resolved::Media)
}

static ROW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?is)<div\s[^>]*\bdata-n\s*=\s*"[^>]*>"#).unwrap());
static ATTR: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"([a-z-]+)="([^"]*)""#).unwrap());

fn songs(html: &str) -> Vec<Song> {
    ROW.find_iter(html)
        .filter_map(|row| {
            let attrs: std::collections::BTreeMap<_, _> = ATTR
                .captures_iter(row.as_str())
                .map(|c| (c[1].to_string(), c[2].to_string()))
                .collect();
            Some(Song {
                index: attrs.get("data-n")?.parse().ok()?,
                title: unescape(attrs.get("data-title")?),
                artist: unescape(attrs.get("data-artist")?),
            })
        })
        .collect()
}

fn unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn select_song(html: &str, index: Option<usize>) -> Result<Song> {
    let songs = songs(html);
    ensure!(!songs.is_empty(), "Glyph share is missing, empty or unavailable");
    ensure!(
        index.is_some() || songs.len() == 1,
        "Glyph share contains multiple songs; choose one with /p/SHARE_ID/listen/INDEX (zero-based); whole playlists are not supported"
    );
    songs
        .into_iter()
        .find(|song| song.index == index.unwrap_or(0))
        .context("no such song in this Glyph share")
}

#[derive(Deserialize)]
struct Source {
    url: String,
    preview: bool,
}

pub async fn extract(client: &Client, target: &Target) -> Result<Media> {
    let page = target.page();
    extract_page(client, &page, target.index).await
}

async fn extract_page(client: &Client, page: &str, index: Option<usize>) -> Result<Media> {
    let html = client.get(page).send().await?.error_for_status()?.text().await?;
    let song = select_song(&html, index)?;
    extract_song(client, page, song).await
}

async fn extract_song(client: &Client, page: &str, song: Song) -> Result<Media> {
    let source: Source = client
        .get(format!("{page}/listen/{}?json=1", song.index))
        .send()
        .await?
        .error_for_status()
        .context("Glyph couldn't provide an audio source for this song")?
        .json()
        .await?;
    let audio = Url::parse(page)?.join(&source.url).context("invalid Glyph audio URL")?;
    ensure!(matches!(audio.scheme(), "http" | "https"), "Glyph audio requires HTTP or HTTPS");
    // Probe the actual source for its MIME type, extension and size, including redirects.
    let media = generic::extract(client, audio.as_str())
        .await
        .context("reading Glyph audio source")?;
    finish(media, page, song, source.preview)
}

fn finish(mut media: Media, page: &str, song: Song, preview: bool) -> Result<Media> {
    ensure!(!media.has_video(), "Glyph source did not return audio");
    media.id = format!("{}-{}", page.rsplit('/').next().unwrap_or_default(), song.index);
    media.title = if preview {
        format!("{} (30 s preview)", song.title)
    } else {
        song.title
    };
    media.uploader = Some(song.artist);
    media.extractor = "Glyph".into();
    media.webpage_url = format!("{page}/listen/{}", song.index);
    media.thumbnail = Some(format!("{page}/cover/{}", song.index));
    if preview {
        media.duration = Some(30.0);
        for stream in &mut media.streams {
            stream.format_id = Some("glyph-preview".into());
        }
    }
    Ok(media)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HTML: &str = r#"<div class="row" data-n="0" data-title="A &amp; B &quot;live&quot;" data-artist="Artist"></div>"#;

    #[tokio::test]
    async fn resolves_web_player_relative_audio_and_probes_the_stream() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let page = format!("http://{}/p/UrPYWeQ4", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for path in ["/p/UrPYWeQ4", "/p/UrPYWeQ4/listen/0?json=1", "/audio"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    assert_eq!(socket.read(&mut byte).await.unwrap(), 1);
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
                let (mime, body, status, extra) = match path {
                    "/audio" => {
                        assert!(request.to_ascii_lowercase().contains("range: bytes=0-0"));
                        ("audio/mpeg", "a", "206 Partial Content", "Content-Range: bytes 0-0/12345\r\n")
                    }
                    "/p/UrPYWeQ4" => ("text/html", HTML, "200 OK", ""),
                    _ => ("application/json", r#"{"url":"/audio","preview":true,"badge":"DZ"}"#, "200 OK", ""),
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let media = extract_page(&Client::new(), &page, None).await.unwrap();
        server.await.unwrap();
        assert_eq!(media.extractor, "Glyph");
        assert_eq!(media.title, "A & B \"live\" (30 s preview)");
        assert_eq!(media.uploader.as_deref(), Some("Artist"));
        assert_eq!(media.streams[0].ext, "mp3");
        assert_eq!(media.streams[0].size, Some(12345));
        assert_eq!(media.streams[0].format_id.as_deref(), Some("glyph-preview"));
        assert_eq!(media.streams[0].url, page.replace("/p/UrPYWeQ4", "/audio"));
    }

    #[test]
    fn recognizes_only_glyph_shares() {
        for url in ["https://glyph.my/p/UrPYWeQ4", "https://www.glyph.my/embed/p/UrPYWeQ4/"] {
            assert_eq!(
                Target::parse(url).unwrap().unwrap(),
                Target {
                    id: "UrPYWeQ4".into(),
                    index: None
                }
            );
        }
        assert_eq!(
            Target::parse("https://glyph.my/p/UrPYWeQ4/listen/2?json=1").unwrap().unwrap().index,
            Some(2)
        );
        assert!(Target::parse("https://glyph.my.evil.test/p/UrPYWeQ4").unwrap().is_none());
        for url in [
            "ftp://glyph.my/p/UrPYWeQ4",
            "https://glyph.my/p/bad",
            "https://glyph.my/p/UrPYWeQ4/listen/-1",
        ] {
            assert!(Target::parse(url).is_err());
        }
        assert!(Target::parse("https://glyph.my/p/UrPYWeQ4/cover").unwrap().is_none());
    }

    #[test]
    fn selects_a_song_without_silently_truncating_a_playlist() {
        let song = select_song(HTML, None).unwrap();
        assert_eq!(song.title, "A & B \"live\"");
        assert_eq!(song.artist, "Artist");
        let playlist = format!("{HTML}<div data-n=\"1\" data-title=\"Second\" data-artist=\"Other\">");
        assert!(select_song(&playlist, None).is_err());
        assert_eq!(select_song(&playlist, Some(1)).unwrap().title, "Second");
        assert!(select_song(HTML, Some(1)).is_err());
        assert!(select_song("<title>Not found</title>", None).is_err());
    }

    #[test]
    fn playlists_keep_order_metadata_and_all_web_entries() {
        let html = format!(
            "{HTML}<div data-n=\"1\" data-title=\"Second\" data-artist=\"Other\"><script type=\"application/ld+json\">{{\"name\":\"My playlist\",\"numTracks\":2}}</script>"
        );
        let list = playlist(&html, "https://glyph.my/p/UrPYWeQ4", "UrPYWeQ4").unwrap().unwrap();
        assert_eq!(list.title, "My playlist");
        assert_eq!(list.entries.len(), 2);
        assert_eq!(list.entries[0].title, "A & B \"live\"");
        assert_eq!(list.entries[1].url, "https://glyph.my/p/UrPYWeQ4/listen/1");
        assert_eq!(list.entries[1].uploader, "Other");
        let json = serde_json::to_value(super::super::Resolved::Playlist(list)).unwrap();
        assert_eq!(json["_type"], "playlist");
        assert_eq!(json["entries"][0]["extractor_key"], "Glyph");
        assert!(json["entries"][0].get("streams").is_none());
        assert!(
            playlist(
                &html.replace("\"numTracks\":2", "\"numTracks\":201"),
                "https://glyph.my/p/UrPYWeQ4",
                "UrPYWeQ4"
            )
            .is_err()
        );
        assert!(playlist(HTML, "https://glyph.my/p/UrPYWeQ4", "UrPYWeQ4").unwrap().is_none());
    }

    #[test]
    fn preserves_audio_and_marks_previews() {
        let media = crate::extract::ytdlp::from_info_json(&serde_json::json!({
            "id": "source", "title": "source", "url": "https://cdn.test/song.mp3",
            "ext": "mp3", "vcodec": "none", "acodec": "mp3"
        }))
        .unwrap();
        for preview in [false, true] {
            let result = finish(
                media.clone(),
                "https://glyph.my/p/UrPYWeQ4",
                select_song(HTML, None).unwrap(),
                preview,
            )
            .unwrap();
            assert_eq!(result.extractor, "Glyph");
            assert_eq!(result.duration, preview.then_some(30.0));
            assert_eq!(result.title.ends_with("(30 s preview)"), preview);
            assert_eq!(result.streams[0].url, media.streams[0].url);
            assert_eq!(result.thumbnail.as_deref(), Some("https://glyph.my/p/UrPYWeQ4/cover/0"));
        }
    }
}
