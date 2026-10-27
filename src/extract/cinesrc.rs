//! CineSrc player resolution through an isolated, temporary Chromium session.
//! The site's own player performs its normal source selection; CDP observes HLS requests.

use crate::media::{Media, Protocol, Stream, StreamKind};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use url::Url;

#[derive(Debug, PartialEq, Eq)]
pub struct Target {
    id: String,
    episode: Option<(u32, u32)>,
}

impl Target {
    pub fn parse(input: &str) -> Result<Option<Self>> {
        let shorthand = input.strip_prefix("cinesrc:").or_else(|| input.strip_prefix("cineby:"));
        let (parts, query): (Vec<String>, Vec<(String, String)>) = if let Some(short) = shorthand {
            (short.split(':').map(str::to_owned).collect(), Vec::new())
        } else {
            let Ok(u) = Url::parse(input) else { return Ok(None) };
            if !matches!(
                u.host_str(),
                Some("cineby.rocks" | "www.cineby.rocks" | "cinesrc.st" | "www.cinesrc.st")
            ) {
                return Ok(None);
            }
            ensure!(matches!(u.scheme(), "http" | "https"), "CineSrc links require HTTP or HTTPS");
            let parts = u
                .path()
                .trim_matches('/')
                .split('/')
                .filter(|p| *p != "embed")
                .map(str::to_owned)
                .collect();
            (parts, u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect())
        };
        let usage = "use cinesrc:movie:TMDB_ID or cinesrc:tv:TMDB_ID:SEASON:EPISODE";
        ensure!(parts.len() >= 2 && matches!(parts[0].as_str(), "movie" | "tv"), "{usage}");
        ensure!(
            !parts[1].is_empty() && parts[1].bytes().all(|b| b.is_ascii_digit()),
            "TMDB ID must be numeric; {usage}"
        );
        let id: u64 = parts[1].parse().context("TMDB ID is too large")?;
        ensure!(id > 0, "TMDB ID must be positive");
        let episode = if parts[0] == "tv" {
            let param = |a: &str, b: &str| query.iter().find(|(k, _)| k == a || k == b).map(|(_, v)| v.as_str());
            let (s, e) = match parts.as_slice() {
                [_, _, s, e] => (s.as_str(), e.as_str()),
                [_, _] => (
                    param("s", "season").context("TV links need a season (s) and episode (e)")?,
                    param("e", "episode").context("TV links need an episode (e)")?,
                ),
                _ => bail!("{usage}"),
            };
            let season = s.parse::<u32>().context("season must be a non-negative number")?;
            let episode = e.parse::<u32>().context("episode must be a positive number")?;
            ensure!(episode > 0, "episode must be positive");
            Some((season, episode))
        } else {
            ensure!(parts.len() == 2, "{usage}");
            None
        };
        Ok(Some(Self {
            id: id.to_string(),
            episode,
        }))
    }

    fn embed(&self) -> String {
        match self.episode {
            Some((s, e)) => format!("https://cinesrc.st/embed/tv/{}?s={s}&e={e}", self.id),
            None => format!("https://cinesrc.st/embed/movie/{}", self.id),
        }
    }

    fn media_id(&self) -> String {
        match self.episode {
            Some((s, e)) => format!("tv-{}-s{s}e{e}", self.id),
            None => format!("movie-{}", self.id),
        }
    }
}

fn browser_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("CHEEK_BROWSER") {
        let path = PathBuf::from(path);
        ensure!(path.is_file(), "CHEEK_BROWSER must point to a Chrome/Chromium executable");
        return Ok(path);
    }
    for name in [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "msedge",
    ] {
        if let Some(path) = super::which(name) {
            return Ok(path);
        }
    }
    let mut paths = vec![
        PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
        PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
    ];
    for root in ["PROGRAMFILES", "PROGRAMFILES(X86)", "LOCALAPPDATA"] {
        if let Some(root) = std::env::var_os(root) {
            for suffix in ["Google/Chrome/Application/chrome.exe", "Microsoft/Edge/Application/msedge.exe"] {
                paths.push(PathBuf::from(&root).join(suffix));
            }
        }
    }
    paths.into_iter().find(|p| p.is_file()).context("CineSrc needs desktop Chrome/Chromium (or Edge); install it or set CHEEK_BROWSER. Android needs a separate browser resolver; a TMDB ID alone cannot resolve there.")
}

struct Browser {
    child: Child,
    // Dropped after the child is stopped, including errors and cancellation.
    profile: tempfile::TempDir,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct Cdp {
    socket: Socket,
    id: u64,
}

impl Cdp {
    async fn next(&mut self) -> Result<Value> {
        loop {
            let message = self.socket.next().await.context("browser disconnected")??;
            if let Message::Text(text) = message {
                return Ok(serde_json::from_str(&text)?);
            }
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.id += 1;
        self.socket
            .send(Message::Text(
                json!({"id": self.id, "method": method, "params": params}).to_string().into(),
            ))
            .await?;
        loop {
            let value = self.next().await?;
            if value["id"].as_u64() == Some(self.id) {
                ensure!(value.get("error").is_none(), "browser command {method} failed: {}", value["error"]);
                return Ok(value["result"].clone());
            }
        }
    }
}

pub async fn extract(client: &Client, target: &Target) -> Result<Media> {
    tokio::time::timeout(Duration::from_secs(90), resolve(client, target))
        .await
        .context("CineSrc did not expose an HLS stream within 90 seconds; the player may be unavailable or require interaction")?
}

async fn resolve(client: &Client, target: &Target) -> Result<Media> {
    let profile = tempfile::Builder::new().prefix("cheek-browser-").tempdir()?;
    let mut command = Command::new(browser_path()?);
    command
        .args([
            "--headless=new",
            "--remote-debugging-address=127.0.0.1",
            "--remote-debugging-port=0",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-background-networking",
            "--autoplay-policy=no-user-gesture-required",
            "--mute-audio",
        ])
        .arg(format!("--user-data-dir={}", profile.path().display()))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    let mut browser = Browser {
        child: command.spawn().context("starting Chromium for CineSrc")?,
        profile,
    };
    let port_file = browser.profile.path().join("DevToolsActivePort");
    let port = loop {
        if let Ok(text) = tokio::fs::read_to_string(&port_file).await
            && let Some(port) = text.lines().next().and_then(|p| p.parse::<u16>().ok())
        {
            break port;
        }
        ensure!(
            browser.child.try_wait()?.is_none(),
            "Chromium exited before its debugging port was ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    // Debugging is loopback-only and uses a new profile, never an existing user session.
    let local = Client::builder().no_proxy().build()?;
    let pages: Vec<Value> = local
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let ws = pages
        .iter()
        .find(|p| p["type"] == "page")
        .and_then(|p| p["webSocketDebuggerUrl"].as_str())
        .context("Chromium did not create a page")?;
    let (socket, _) = connect_async(ws).await?;
    let mut cdp = Cdp { socket, id: 0 };
    cdp.call("Network.enable", json!({})).await?;
    let version = cdp.call("Browser.getVersion", json!({})).await?;
    let ua = version["userAgent"]
        .as_str()
        .context("missing browser version")?
        .replace("HeadlessChrome/", "Chrome/");
    cdp.call("Network.setUserAgentOverride", json!({"userAgent": ua})).await?;
    cdp.call("Page.enable", json!({})).await?;
    let navigation = cdp.call("Page.navigate", json!({"url": target.embed()})).await?;
    ensure!(
        navigation.get("errorText").is_none(),
        "CineSrc navigation failed: {}",
        navigation["errorText"]
    );
    let frame_id = navigation["frameId"].as_str().context("missing player frame")?.to_string();
    let mut requests = BTreeMap::new();
    let (stream_url, mut headers) = loop {
        let event = cdp.next().await?;
        let p = &event["params"];
        match event["method"].as_str() {
            Some("Network.requestWillBeSent") => {
                if let Some(id) = p["requestId"].as_str() {
                    requests.insert(id.to_string(), p["request"]["headers"].clone());
                }
            }
            Some("Network.loadingFinished" | "Network.loadingFailed") => {
                if let Some(id) = p["requestId"].as_str() {
                    requests.remove(id);
                }
            }
            Some("Network.responseReceived") => {
                let response = &p["response"];
                let is_playlist = response["url"]
                    .as_str()
                    .and_then(|u| Url::parse(u).ok())
                    .is_some_and(|u| u.path().ends_with(".m3u8"));
                if p["frameId"].as_str() == Some(frame_id.as_str()) && is_playlist {
                    ensure!(
                        response["status"].as_u64().is_some_and(|s| (200..300).contains(&s)),
                        "CineSrc's playlist endpoint returned HTTP {}; the selected server is unavailable or denied access",
                        response["status"]
                    );
                }
                if p["frameId"].as_str() == Some(frame_id.as_str())
                    && response["status"].as_u64().is_some_and(|s| (200..300).contains(&s))
                    && (is_playlist
                        || response["mimeType"]
                            .as_str()
                            .is_some_and(|mime| mime.to_ascii_lowercase().contains("mpegurl")))
                    && let Some(url) = response["url"].as_str()
                {
                    let mut headers = BTreeMap::new();
                    if let Some(found) = p["requestId"].as_str().and_then(|id| requests.get(id)).and_then(Value::as_object) {
                        for (k, v) in found {
                            if ["referer", "origin", "user-agent"].iter().any(|h| k.eq_ignore_ascii_case(h))
                                && let Some(v) = v.as_str()
                            {
                                headers.insert(k.to_ascii_lowercase(), v.to_string());
                            }
                        }
                    }
                    break (url.to_string(), headers);
                }
            }
            _ => {}
        }
    };
    let metadata = cdp
        .call(
            "Runtime.evaluate",
            json!({"expression": "({title:document.title,ua:navigator.userAgent})", "returnByValue":true}),
        )
        .await?;
    headers.insert(
        "user-agent".into(),
        metadata["result"]["value"]["ua"]
            .as_str()
            .context("missing browser user agent")?
            .into(),
    );
    headers.entry("referer".into()).or_insert_with(|| target.embed());
    let cookies = cdp.call("Network.getCookies", json!({"urls":[stream_url]})).await?;
    let cookie = cookies["cookies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| Some(format!("{}={}", c["name"].as_str()?, c["value"].as_str()?)))
        .collect::<Vec<_>>()
        .join("; ");
    if !cookie.is_empty() {
        headers.insert("cookie".into(), cookie);
    }
    // Confirm that the observed URL also works outside the browser before returning it.
    let mut request = client.get(&stream_url);
    for (k, v) in &headers {
        request = request.header(k, v);
    }
    let playlist = request.send().await?.error_for_status()?.text().await?;
    ensure!(
        playlist.trim_start_matches('\u{feff}').trim_start().starts_with("#EXTM3U"),
        "CineSrc returned a stream that is not an HLS playlist"
    );
    let title = metadata["result"]["value"]["title"]
        .as_str()
        .filter(|t| !t.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| target.media_id());
    // Close all Chromium subprocesses before the temporary profile is removed.
    let _ = cdp.call("Browser.close", json!({})).await;
    Ok(Media {
        id: target.media_id(),
        title,
        extractor: "CineSrc".into(),
        webpage_url: target.embed(),
        thumbnail: None,
        duration: None,
        uploader: None,
        streams: vec![Stream {
            url: stream_url,
            ext: "mp4".into(),
            kind: StreamKind::Muxed,
            protocol: Protocol::Hls,
            format_id: Some("hls".into()),
            size: None,
            codec: None,
            height: None,
            headers,
            chunk_limit: None,
            fragments: Vec::new(),
        }],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_links_normalize_to_documented_embeds() {
        for input in [
            "cinesrc:movie:42",
            "cineby:movie:42",
            "https://cineby.rocks/movie/42",
            "https://cinesrc.st/embed/movie/42",
        ] {
            assert_eq!(Target::parse(input).unwrap().unwrap().embed(), "https://cinesrc.st/embed/movie/42");
        }
        for input in [
            "cinesrc:tv:1396:1:2",
            "https://cineby.rocks/tv/1396?s=1&e=2",
            "https://cinesrc.st/embed/tv/1396?season=1&episode=2",
        ] {
            assert_eq!(
                Target::parse(input).unwrap().unwrap().embed(),
                "https://cinesrc.st/embed/tv/1396?s=1&e=2"
            );
        }
    }

    #[test]
    fn rejects_ambiguous_or_invalid_targets() {
        for input in [
            "cinesrc:movie:0",
            "cinesrc:movie:no",
            "cinesrc:tv:42",
            "cinesrc:tv:42:1:0",
            "cinesrc:movie:42:1",
            "https://cineby.rocks/",
        ] {
            assert!(Target::parse(input).is_err(), "{input}");
        }
        assert!(Target::parse("https://cineby.rocks.evil.test/movie/42").unwrap().is_none());
        assert!(Target::parse("https://example.com/movie/42").unwrap().is_none());
        assert!(Target::parse("42").unwrap().is_none());
        assert!(Target::parse("cinesrc:tv:42:0:1").unwrap().is_some());
    }
}
