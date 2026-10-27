//! End to end against a small local server that answers byte ranges.

use cheek::download::{Options, Shared, fetch};
use cheek::events::Progress;
use cheek::{Protocol, Stream, StreamKind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

fn body(len: usize) -> Arc<Vec<u8>> {
    Arc::new((0..len).map(|i| (i * 7 % 251) as u8).collect())
}

/// Serves `data`; with `ranges` false it ignores Range headers. `fail_every` drops every
/// n-th ranged request half way to exercise retries.
async fn serve(data: Arc<Vec<u8>>, ranges: bool, fail_every: usize) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (data, counter) = (data.clone(), counter.clone());
            tokio::spawn(async move {
                loop {
                    let mut buf = Vec::new();
                    let mut byte = [0u8; 1];
                    while !buf.ends_with(b"\r\n\r\n") {
                        if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        buf.push(byte[0]);
                    }
                    let req = String::from_utf8_lossy(&buf).to_ascii_lowercase();
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    let range = req.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(|r| {
                        let (a, b) = r.trim().split_once('-').unwrap();
                        let a: usize = a.parse().unwrap();
                        let b: usize = if b.is_empty() { data.len() - 1 } else { b.parse().unwrap() };
                        (a, b.min(data.len() - 1))
                    });
                    match range.filter(|_| ranges) {
                        Some((a, b)) => {
                            let head = format!(
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{}\r\nContent-Type: video/mp4\r\n\r\n",
                                b - a + 1,
                                data.len()
                            );
                            sock.write_all(head.as_bytes()).await.ok();
                            let slice = &data[a..=b];
                            if fail_every > 0 && n % fail_every == 0 && slice.len() > 2 {
                                sock.write_all(&slice[..slice.len() / 2]).await.ok();
                                return; // connection drops mid-body
                            }
                            sock.write_all(slice).await.ok();
                        }
                        None => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp4\r\n\r\n",
                                data.len()
                            );
                            sock.write_all(head.as_bytes()).await.ok();
                            sock.write_all(&data).await.ok();
                        }
                    }
                }
            });
        }
    });
    (format!("http://{addr}/video.mp4"), hits)
}

fn stream(url: String) -> Stream {
    Stream {
        url,
        ext: "mp4".into(),
        kind: StreamKind::Muxed,
        protocol: Protocol::Http,
        format_id: None,
        size: None,
        codec: None,
        height: None,
        headers: Default::default(),
        chunk_limit: None,
        fragments: Vec::new(),
    }
}

fn shared(connections: usize) -> Shared {
    Shared {
        client: cheek::http::download_client(None, connections).unwrap(),
        permits: Arc::new(Semaphore::new(connections)),
        progress: Arc::new(Progress::default()),
        options: Options {
            connections,
            chunk_size: 1 << 20,
        },
    }
}

#[tokio::test]
async fn ranged_download_is_byte_exact() {
    let data = body(9 * 1024 * 1024 + 123);
    let (url, hits) = serve(data.clone(), true, 0).await;
    let dir = tempdir();
    let dest = dir.join("a.mp4");
    let s = shared(8);
    let n = fetch(&s, &stream(url), &dest).await.unwrap();
    assert_eq!(n as usize, data.len());
    assert_eq!(std::fs::read(&dest).unwrap(), *data);
    assert!(hits.load(Ordering::SeqCst) > 8, "should have used many range requests");
    assert_eq!(s.progress.snapshot(), (data.len() as u64, data.len() as u64));
    assert!(!dir.join("a.mp4.cheek").exists());
}

#[tokio::test]
async fn retries_dropped_connections() {
    let data = body(5 * 1024 * 1024);
    let (url, _) = serve(data.clone(), true, 3).await;
    let dir = tempdir();
    let dest = dir.join("b.mp4");
    let s = shared(4);
    fetch(&s, &stream(url), &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), *data);
    assert_eq!(s.progress.snapshot().0, data.len() as u64, "rewound bytes must not count twice");
}

#[tokio::test]
async fn falls_back_without_ranges() {
    let data = body(2 * 1024 * 1024 + 5);
    let (url, hits) = serve(data.clone(), false, 0).await;
    let dir = tempdir();
    let dest = dir.join("c.mp4");
    fetch(&shared(8), &stream(url), &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), *data);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "reuse the full response from the probe");
}

#[tokio::test]
async fn respects_small_chunk_limits() {
    let data = body(256 * 1024);
    let (url, hits) = serve(data.clone(), true, 0).await;
    let mut stream = stream(url);
    stream.chunk_limit = Some(64 << 10);
    let dest = tempdir().join("small.mp4");
    fetch(&shared(2), &stream, &dest).await.unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), *data);
    assert_eq!(hits.load(Ordering::SeqCst), 5);
}

#[tokio::test]
async fn resumes_from_the_sidecar() {
    let data = body(6 * 1024 * 1024);
    let (url, hits) = serve(data.clone(), true, 0).await;
    let dir = tempdir();
    let dest = dir.join("d.mp4");
    // First run: everything; then pretend only half was there and the rest got lost.
    let s = shared(4);
    fetch(&s, &stream(url.clone()), &dest).await.unwrap();
    let chunk = (data.len() as u64 / 16).clamp(512 << 10, 1 << 20);
    let count = (data.len() as u64).div_ceil(chunk) as usize;
    let done: Vec<bool> = (0..count).map(|i| i < count / 2).collect();
    std::fs::write(
        dir.join("d.mp4.cheek"),
        serde_json::json!({"total": data.len(), "chunk": chunk, "done": done}).to_string(),
    )
    .unwrap();
    let mut f = std::fs::OpenOptions::new().write(true).open(&dest).unwrap();
    use std::io::{Seek, Write};
    f.seek(std::io::SeekFrom::Start(chunk * (count / 2) as u64)).unwrap();
    f.write_all(&vec![0u8; (data.len() as u64 - chunk * (count / 2) as u64) as usize])
        .unwrap();
    drop(f);
    let before = hits.load(Ordering::SeqCst);
    fetch(&shared(4), &stream(url), &dest).await.unwrap();
    let requests = hits.load(Ordering::SeqCst) - before;
    assert_eq!(std::fs::read(&dest).unwrap(), *data);
    assert_eq!(requests, 1 + count - count / 2, "only the missing chunks plus the probe");
}

#[tokio::test]
async fn resumes_with_a_different_connection_count() {
    let data = body(6 * 1024 * 1024);
    let (url, hits) = serve(data.clone(), true, 0).await;
    let dir = tempdir();
    let dest = dir.join("e.mp4");
    // Notes from a run with other chunking than one connection would pick now.
    let chunk = 512u64 << 10;
    let count = (data.len() as u64).div_ceil(chunk) as usize;
    let kept = count * 3 / 4;
    let mut partial = data[..chunk as usize * kept].to_vec();
    partial.resize(data.len(), 0);
    std::fs::write(&dest, partial).unwrap();
    let done: Vec<bool> = (0..count).map(|i| i < kept).collect();
    std::fs::write(
        dir.join("e.mp4.cheek"),
        serde_json::json!({"total": data.len(), "chunk": chunk, "done": done}).to_string(),
    )
    .unwrap();
    fetch(&shared(1), &stream(url), &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), *data);
    assert_eq!(hits.load(Ordering::SeqCst), 1 + count - kept, "the notes' chunks, not new ones");
}

fn tempdir() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("cheek-test-{}-{}", std::process::id(), rand_suffix()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn rand_suffix() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    static N: AtomicUsize = AtomicUsize::new(0);
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64 ^ (N.fetch_add(1, Ordering::SeqCst) as u64) << 48
}
