//! Turning a link into `Media`.

pub mod generic;
pub mod ytdlp;

use crate::media::Media;
use anyhow::{Result, bail};
use reqwest::Client;
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Resolver {
    /// Cheek's own extractor first, yt-dlp for the sites it can't read.
    Auto,
    /// Only cheek's own extractor.
    Native,
    /// Always ask yt-dlp.
    YtDlp,
}

pub struct ResolveOptions {
    pub resolver: Resolver,
    /// yt-dlp to run; `None` looks for it on `PATH`.
    pub yt_dlp: Option<PathBuf>,
    /// yt-dlp format selector, used when yt-dlp resolves.
    pub format: String,
}

/// Sites that only make sense through yt-dlp's extractors.
const YT_DLP_HOSTS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "instagram.com",
    "tiktok.com",
    "twitter.com",
    "x.com",
    "soundcloud.com",
    "twitch.tv",
    "reddit.com",
    "redd.it",
    "vimeo.com",
    "facebook.com",
    "fb.watch",
    "bilibili.com",
    "dailymotion.com",
    "bandcamp.com",
    "pinterest.com",
    "threads.net",
    "bsky.app",
    "nicovideo.jp",
    "spotify.com",
];

fn wants_yt_dlp(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .is_some_and(|h| YT_DLP_HOSTS.iter().any(|d| h == *d || h.ends_with(&format!(".{d}"))))
}

pub fn find_yt_dlp(explicit: Option<&PathBuf>) -> Option<PathBuf> {
    explicit.cloned().or_else(|| which("yt-dlp"))
}

pub fn which(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find_map(|dir| exts.iter().map(|e| dir.join(format!("{name}{e}"))).find(|p| p.is_file()))
}

pub async fn resolve(client: &Client, url: &str, opts: &ResolveOptions) -> Result<Media> {
    let yt_dlp = || find_yt_dlp(opts.yt_dlp.as_ref());
    match opts.resolver {
        Resolver::Native => generic::extract(client, url).await,
        Resolver::YtDlp => match yt_dlp() {
            Some(bin) => ytdlp::resolve(&bin, url, &opts.format).await,
            None => bail!("yt-dlp isn't installed; get it from https://github.com/yt-dlp/yt-dlp or pass --yt-dlp"),
        },
        Resolver::Auto => {
            if !wants_yt_dlp(url) {
                match generic::extract(client, url).await {
                    Ok(m) => return Ok(m),
                    Err(e) if e.downcast_ref::<generic::Unsupported>().is_none() => return Err(e),
                    Err(_) => {}
                }
            }
            match yt_dlp() {
                Some(bin) => ytdlp::resolve(&bin, url, &opts.format).await,
                None => bail!("cheek can't read this site by itself and yt-dlp isn't installed (https://github.com/yt-dlp/yt-dlp)"),
            }
        }
    }
}
