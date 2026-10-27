//! Real HTTP requests: parallel fragments must still produce byte-exact ordered output.
use cheek::download::{Options, Shared};
use cheek::events::Progress;
use cheek::extract::ytdlp::from_info_json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

#[tokio::test]
async fn parallel_fragments_preserve_order_and_retry() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let retries = Arc::new(AtomicUsize::new(0));
    let (a, p, r) = (active.clone(), peak.clone(), retries.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (a, p, r) = (a.clone(), p.clone(), r.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.contains("x-fragment-test: yes"));
                let i: usize = request.split_whitespace().nth(1).unwrap().trim_start_matches('/').parse().unwrap();
                let n = a.fetch_add(1, Ordering::SeqCst) + 1;
                p.fetch_max(n, Ordering::SeqCst);
                // The initialization fragment finishes after later segments.
                tokio::time::sleep(Duration::from_millis(if i == 0 { 100 } else { 15 })).await;
                let fail = i == 2 && r.fetch_add(1, Ordering::SeqCst) == 0;
                let body = if fail { Vec::new() } else { vec![i as u8; 70_000 + i] };
                let status = if fail { "503 Service Unavailable" } else { "200 OK" };
                let head = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
                sock.write_all(head.as_bytes()).await.unwrap();
                sock.write_all(&body).await.unwrap();
                a.fetch_sub(1, Ordering::SeqCst);
            });
        }
    });
    let media = from_info_json(&serde_json::json!({
        "url": format!("{base}manifest.mpd"), "fragment_base_url": base,
        "protocol": "http_dash_segments", "vcodec": "none", "acodec": "aac", "ext": "m4a",
        "http_headers": {"X-Fragment-Test": "yes"},
        "fragments": (0..6).map(|i| serde_json::json!({"path": i.to_string()})).collect::<Vec<_>>()
    }))
    .unwrap();
    let shared = Shared {
        client: cheek::http::download_client(None, 3).unwrap(),
        permits: Arc::new(Semaphore::new(3)),
        progress: Arc::new(Progress::default()),
        options: Options {
            connections: 3,
            ..Options::default()
        },
    };
    let dest = std::env::temp_dir().join(format!("cheek-fragments-{}.m4a", std::process::id()));
    let stream = &media.streams[0];
    let size = cheek::hls::fetch_parts(&shared, stream, &dest, stream.fragments.clone())
        .await
        .unwrap();
    let expected: Vec<u8> = (0..6).flat_map(|i| vec![i as u8; 70_000 + i]).collect();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert_eq!(size, expected.len() as u64);
    assert_eq!(shared.progress.snapshot().0, size);
    assert!(
        (2..=3).contains(&peak.load(Ordering::SeqCst)),
        "must be parallel and respect the shared limit"
    );
    assert_eq!(retries.load(Ordering::SeqCst), 2);
    // Exercise the real job dispatch and audio-only finalization, without ffmpeg.
    let out_dir = std::env::temp_dir().join(format!("cheek-fragment-job-{}", std::process::id()));
    let final_path = cheek::run(
        cheek::http::download_client(None, 3).unwrap(),
        cheek::Job {
            media,
            out_dir: out_dir.clone(),
            temp_dir: None,
            template: "audio.%(ext)s".into(),
            audio: Some("best".into()),
            merge_format: None,
            max_height: None,
            ffmpeg: None,
            embed_thumbnail: false,
            embed_metadata: false,
            options: Options {
                connections: 3,
                ..Options::default()
            },
        },
        &cheek::events::Reporter::new(false, true),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(&final_path).unwrap(), expected);
    std::fs::remove_file(final_path).unwrap();
    std::fs::remove_dir(out_dir).unwrap();
    std::fs::remove_file(dest).unwrap();
    server.abort();
}

#[tokio::test]
async fn missing_fragment_fails_without_retries() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/missing", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let counter = hits.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, Ordering::SeqCst);
            let mut byte = [0];
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            sock.write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        }
    });
    let media = from_info_json(&serde_json::json!({
        "url": url, "protocol": "http_dash_segments", "fragments": [{"url": url}]
    }))
    .unwrap();
    let shared = Shared {
        client: cheek::http::download_client(None, 1).unwrap(),
        permits: Arc::new(Semaphore::new(1)),
        progress: Arc::new(Progress::default()),
        options: Options::default(),
    };
    let dest = std::env::temp_dir().join(format!("cheek-fragment-failure-{}", std::process::id()));
    let s = &media.streams[0];
    assert!(cheek::hls::fetch_parts(&shared, s, &dest, s.fragments.clone()).await.is_err());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    std::fs::remove_file(dest).unwrap();
    server.abort();
}

#[tokio::test]
async fn interrupted_fragments_resume_where_they_stopped() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    // Fragment 4 is missing on the first run; every request is counted by index.
    let broken = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let hits: Arc<Vec<AtomicUsize>> = Arc::new((0..6).map(|_| AtomicUsize::new(0)).collect());
    let (b, h) = (broken.clone(), hits.clone());
    let server = tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (b, h) = (b.clone(), h.clone());
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    if sock.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap();
                let i: usize = path.trim_start_matches('/').split('?').next().unwrap().parse().unwrap();
                h[i].fetch_add(1, Ordering::SeqCst);
                let missing = i == 4 && b.load(Ordering::SeqCst);
                let body = if missing { Vec::new() } else { vec![i as u8 + 1; 50_000 + i] };
                let status = if missing { "404 Not Found" } else { "200 OK" };
                let head = format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n", body.len());
                sock.write_all(head.as_bytes()).await.unwrap();
                sock.write_all(&body).await.unwrap();
            });
        }
    });
    let shared = || Shared {
        client: cheek::http::download_client(None, 1).unwrap(),
        permits: Arc::new(Semaphore::new(1)),
        progress: Arc::new(Progress::default()),
        options: Options {
            connections: 1,
            ..Options::default()
        },
    };
    // The second resolve hands out fresh tokens for the same fragments.
    let parts = |token: &str| (0..6).map(|i| format!("{base}{i}?token={token}")).collect::<Vec<_>>();
    let media = from_info_json(&serde_json::json!({
        "url": format!("{base}manifest.mpd"), "protocol": "http_dash_segments", "vcodec": "none", "acodec": "aac", "ext": "m4a",
        "fragments": [{"url": format!("{base}0")}]
    }))
    .unwrap();
    let stream = &media.streams[0];
    let dest = std::env::temp_dir().join(format!("cheek-fragment-resume-{}.m4a", std::process::id()));
    let notes = std::path::PathBuf::from(format!("{}.cheek", dest.display()));

    assert!(cheek::hls::fetch_parts(&shared(), stream, &dest, parts("a")).await.is_err());
    assert!(notes.exists(), "the first run leaves its notes");

    broken.store(false, Ordering::SeqCst);
    let again = shared();
    let size = cheek::hls::fetch_parts(&again, stream, &dest, parts("b")).await.unwrap();
    let expected: Vec<u8> = (0..6).flat_map(|i| vec![i as u8 + 1; 50_000 + i]).collect();
    assert_eq!(std::fs::read(&dest).unwrap(), expected);
    assert_eq!(size, expected.len() as u64);
    assert_eq!(again.progress.snapshot().0, size, "kept fragments count as done");
    for i in 0..4 {
        assert_eq!(hits[i].load(Ordering::SeqCst), 1, "fragment {i} was fetched again");
    }
    assert!(!notes.exists(), "a finished download drops its notes");
    std::fs::remove_file(dest).unwrap();
    server.abort();
}
