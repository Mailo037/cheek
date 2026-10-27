//! Parallel ranged HTTP downloads.
//!
//! A file is cut into chunks; workers take the next missing chunk, fetch it with a
//! `Range` request and write it straight to its offset. Finished chunks are recorded in a
//! small sidecar file, so an interrupted download picks up where it stopped. Servers that
//! don't do ranges get one plain stream.

use crate::events::Progress;
use crate::http;
use crate::media::Stream;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use reqwest::{Client, Response, StatusCode, header};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt, BufWriter};
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct Options {
    /// Requests in flight across all streams of one download.
    pub connections: usize,
    /// Upper bound for one range request.
    pub chunk_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            connections: 8,
            chunk_size: 8 << 20,
        }
    }
}

const MIN_CHUNK: u64 = 512 << 10;
const RETRIES: u32 = 6;

/// Shared by every stream of a download, so video and audio split the connections.
pub struct Shared {
    pub client: Client,
    pub permits: Arc<Semaphore>,
    pub progress: Arc<Progress>,
    pub options: Options,
}

/// Downloads one HTTP stream to `dest`.
pub async fn fetch(shared: &Shared, stream: &Stream, dest: &Path) -> Result<u64> {
    let permit = shared.permits.acquire().await?;
    let resp = probe(&shared.client, stream).await?;
    if resp.status() == StatusCode::PARTIAL_CONTENT {
        let total = resp
            .headers()
            .get(header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok())
            .or(stream.size)
            .filter(|n| *n > 0)
            .context("range response has no file size")?;
        // Drain the one-byte probe so its connection can be reused.
        resp.bytes().await?;
        drop(permit);
        ranged_fetch(shared, stream, dest, total).await
    } else {
        // A server ignoring Range already sent the whole file. Keep that response.
        single_fetch(shared, dest, resp, stream.size).await
    }
}

/// Size, and whether the server answers byte ranges.
async fn probe(client: &Client, stream: &Stream) -> Result<Response> {
    let resp = http::get(client, stream)
        .header(header::RANGE, "bytes=0-0")
        .send()
        .await
        .context("connecting")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("server said {status}");
    }
    Ok(resp)
}

#[derive(Serialize, Deserialize)]
struct State {
    total: u64,
    chunk: u64,
    done: Vec<bool>,
}

fn state_path(dest: &Path) -> PathBuf {
    let mut p = dest.as_os_str().to_owned();
    p.push(".cheek");
    PathBuf::from(p)
}

async fn ranged_fetch(shared: &Shared, stream: &Stream, dest: &Path, total: u64) -> Result<u64> {
    let opts = &shared.options;
    let limit = stream.chunk_limit.unwrap_or(u64::MAX).min(opts.chunk_size).max(1);
    // Enough chunks that every connection stays busy until the end, but none above the limit.
    let chunk = (total / (opts.connections.clamp(1, 64) as u64 * 4)).clamp(MIN_CHUNK.min(limit), limit);
    let count = total.div_ceil(chunk) as usize;

    let sidecar = state_path(dest);
    let mut done = vec![false; count];
    // Resume only when the notes describe exactly this file and chunking.
    if dest.exists()
        && let Ok(s) = tokio::fs::read(&sidecar)
            .await
            .map_err(anyhow::Error::from)
            .and_then(|b| Ok(serde_json::from_slice::<State>(&b)?))
        && s.total == total
        && s.chunk == chunk
        && s.done.len() == count
    {
        done = s.done;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    file.set_len(total).await?;
    drop(file);

    let size_of = |i: usize| (chunk * (i as u64 + 1)).min(total) - chunk * i as u64;
    shared.progress.add_total(total);
    shared
        .progress
        .add(done.iter().enumerate().filter(|(_, d)| **d).map(|(i, _)| size_of(i)).sum());

    let queue: Arc<Mutex<VecDeque<usize>>> = Arc::new(Mutex::new((0..count).filter(|i| !done[*i]).collect()));
    let state = Arc::new(Mutex::new((State { total, chunk, done }, Instant::now())));
    let workers = opts.connections.clamp(1, 64).min(queue.lock().unwrap().len().max(1));

    // Dropping the parent future must also cancel workers writing its files.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..workers {
        let (queue, state, sidecar) = (queue.clone(), state.clone(), sidecar.clone());
        let (client, permits, progress) = (shared.client.clone(), shared.permits.clone(), shared.progress.clone());
        let (stream, dest) = (stream.clone(), dest.to_path_buf());
        tasks.spawn(async move {
            let mut file = BufWriter::with_capacity(256 << 10, OpenOptions::new().write(true).open(&dest).await?);
            loop {
                let Some(i) = queue.lock().unwrap().pop_front() else { break };
                let _permit = permits.acquire().await?;
                let start = chunk * i as u64;
                let end = (start + chunk).min(total) - 1;
                fetch_range(&client, &stream, &mut file, start, end, &progress)
                    .await
                    .with_context(|| format!("bytes {start}-{end}"))?;
                // A completed chunk must be on disk before its resume marker is saved.
                file.flush().await?;
                // Note the chunk; rewrite the sidecar at most once a second.
                let snapshot = {
                    let mut s = state.lock().unwrap();
                    s.0.done[i] = true;
                    if s.1.elapsed() > Duration::from_secs(1) {
                        s.1 = Instant::now();
                        Some(serde_json::to_vec(&s.0)?)
                    } else {
                        None
                    }
                };
                if let Some(bytes) = snapshot {
                    tokio::fs::write(&sidecar, bytes).await.ok();
                }
            }
            file.flush().await?;
            anyhow::Ok(())
        });
    }
    let mut failure = None;
    while let Some(t) = tasks.join_next().await {
        if let Err(e) = t.map_err(anyhow::Error::from).and_then(|r| r) {
            failure.get_or_insert(e);
        }
    }
    if let Some(e) = failure {
        let bytes = serde_json::to_vec(&state.lock().unwrap().0)?;
        tokio::fs::write(&sidecar, bytes).await.ok();
        return Err(e);
    }
    tokio::fs::remove_file(&sidecar).await.ok();
    Ok(total)
}

async fn fetch_range(
    client: &Client,
    stream: &Stream,
    file: &mut BufWriter<File>,
    start: u64,
    end: u64,
    progress: &Progress,
) -> Result<()> {
    let want = end - start + 1;
    let mut attempt = 0;
    loop {
        let mut written = 0u64;
        let result: Result<()> = async {
            let resp = http::get(client, stream)
                .header(header::RANGE, format!("bytes={start}-{end}"))
                .send()
                .await?;
            let status = resp.status();
            if status != StatusCode::PARTIAL_CONTENT {
                return Err(anyhow!(HttpStatus(status)));
            }
            file.seek(std::io::SeekFrom::Start(start)).await?;
            let mut body = resp.bytes_stream();
            while let Some(piece) = body.next().await {
                let piece = piece?;
                let take = (piece.len() as u64).min(want - written);
                file.write_all(&piece[..take as usize]).await?;
                written += take;
                progress.add(take);
                if written == want {
                    break;
                }
            }
            if written < want {
                bail!("connection closed after {written} of {want} bytes");
            }
            Ok(())
        }
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                progress.rewind(written);
                attempt += 1;
                let fatal = e
                    .downcast_ref::<HttpStatus>()
                    .is_some_and(|s| s.0.is_client_error() && s.0 != StatusCode::TOO_MANY_REQUESTS);
                if fatal || attempt >= RETRIES {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_millis(300 * 2u64.pow(attempt))).await;
            }
        }
    }
}

#[derive(Debug)]
struct HttpStatus(StatusCode);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "server said {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

/// One plain request, for servers without range support.
async fn single_fetch(shared: &Shared, dest: &Path, resp: Response, total: Option<u64>) -> Result<u64> {
    if let Some(t) = resp.content_length().or(total) {
        shared.progress.add_total(t);
    }
    let mut file = BufWriter::with_capacity(256 << 10, File::create(dest).await?);
    let mut body = resp.bytes_stream();
    let mut written = 0u64;
    while let Some(piece) = body.next().await {
        let piece = piece?;
        file.write_all(&piece).await?;
        written += piece.len() as u64;
        shared.progress.add(piece.len() as u64);
        shared.progress.set_total_at_least(written);
    }
    file.flush().await?;
    Ok(written)
}
