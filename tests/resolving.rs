//! Resolution request counts and connection reuse through the real CLI.
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn serve(ranged_html: bool) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let paths = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));
    let (seen, count) = (paths.clone(), connections.clone());
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            count.fetch_add(1, Ordering::SeqCst);
            let seen = seen.clone();
            tokio::spawn(async move {
                loop {
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                            return;
                        }
                        request.push(byte[0]);
                    }
                    let text = String::from_utf8(request).unwrap().to_lowercase();
                    let path = text.split_whitespace().nth(1).unwrap().to_owned();
                    seen.lock().unwrap().push(path.clone());
                    let html = br#"<title>Song</title><meta property="og:audio" content="/audio.mp3">"#;
                    let is_page = path == "/page";
                    let data = if is_page { html.to_vec() } else { vec![90; 4 * 1024 * 1024] };
                    let range = text.lines().find_map(|l| l.strip_prefix("range: bytes="));
                    let (a, b, ranged) = if let Some(range) = range.filter(|_| !is_page || ranged_html) {
                        let (a, b) = range.trim().split_once('-').unwrap();
                        (a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap().min(data.len() - 1), true)
                    } else {
                        (0, data.len() - 1, false)
                    };
                    let status = if ranged { "206 Partial Content" } else { "200 OK" };
                    let mime = if is_page { "text/html" } else { "audio/mpeg" };
                    let extra = if ranged {
                        format!("Content-Range: bytes {a}-{b}/{}\r\n", data.len())
                    } else {
                        String::new()
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\n{extra}\r\n",
                        b - a + 1
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() || socket.write_all(&data[a..=b]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (origin, paths, connections, handle)
}

#[tokio::test]
async fn full_html_response_is_not_downloaded_again() {
    let (origin, paths, _, server) = serve(false).await;
    let media = cheek::extract::generic::extract(&reqwest::Client::new(), &format!("{origin}/page"))
        .await
        .unwrap();
    assert_eq!(media.title, "Song");
    assert_eq!(paths.lock().unwrap().iter().filter(|p| *p == "/page").count(), 1);
    server.abort();
}

#[tokio::test]
async fn partial_html_still_gets_its_full_page() {
    let (origin, paths, _, server) = serve(true).await;
    let media = cheek::extract::generic::extract(&reqwest::Client::new(), &format!("{origin}/page"))
        .await
        .unwrap();
    assert_eq!(media.streams[0].size, Some(4 * 1024 * 1024));
    assert_eq!(paths.lock().unwrap().iter().filter(|p| *p == "/page").count(), 2);
    server.abort();
}

#[tokio::test]
async fn cli_reuses_resolution_connection_for_a_four_mb_download() {
    let (origin, _, connections, server) = serve(false).await;
    let dir = tempfile::tempdir().unwrap();
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_cheek"))
        .args(["--resolver", "native", "--json", "-N", "1", "-o", "song.mp3", "-P"])
        .arg(dir.path())
        .arg(format!("{origin}/page"))
        .output()
        .await
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(dir.path().join("song.mp3")).unwrap(), vec![90; 4 * 1024 * 1024]);
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "one pooled HTTP connection from page to finished file"
    );
    let events: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    for phase in ["resolve", "download", "postprocess"] {
        assert!(events.iter().any(|e| e["type"] == "timing" && e["phase"] == phase));
    }
    server.abort();
}
