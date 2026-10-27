//! Exercise playlist JSON through the real CLI, including a failed middle entry.
use cheek::extract::glyph::{Playlist, PlaylistEntry};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn playlist_json_downloads_in_order_and_continues_after_missing_audio() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                request.push(byte[0]);
            }
            let missing = String::from_utf8_lossy(&request).contains("/missing.mp3 ");
            let status = if missing { "404 Not Found" } else { "200 OK" };
            let body = if missing { "" } else { "mp3" };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: audio/mpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.ok();
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let json = dir.path().join("playlist.json");
    let playlist = Playlist {
        kind: "playlist".into(),
        id: "UrPYWeQ4".into(),
        title: "Playlist".into(),
        extractor: "Glyph".into(),
        webpage_url: "https://glyph.my/p/UrPYWeQ4".into(),
        thumbnail: "https://glyph.my/p/UrPYWeQ4/cover".into(),
        entries: ["first", "missing", "last"]
            .into_iter()
            .map(|name| PlaylistEntry {
                id: name.into(),
                title: name.into(),
                uploader: "Artist".into(),
                url: format!("{origin}/{name}.mp3"),
                thumbnail: String::new(),
                extractor_key: "Glyph".into(),
            })
            .collect(),
    };
    std::fs::write(&json, serde_json::to_vec(&playlist).unwrap()).unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_cheek"))
        .args(["--info-json"])
        .arg(&json)
        .args(["--resolver", "native", "--json", "-o", "%(title)s.%(ext)s", "-P"])
        .arg(dir.path())
        .output()
        .await
        .unwrap();
    server.abort();
    assert!(!output.status.success(), "missing entries must produce a failing exit status");
    assert_eq!(std::fs::read(dir.path().join("first.mp3")).unwrap(), b"mp3");
    assert_eq!(std::fs::read(dir.path().join("last.mp3")).unwrap(), b"mp3");
    let events: Vec<serde_json::Value> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let done: Vec<_> = events.iter().filter(|e| e["type"] == "done").collect();
    assert_eq!(done.len(), 2);
    assert!(done[0]["path"].as_str().unwrap().ends_with("first.mp3"));
    assert!(done[1]["path"].as_str().unwrap().ends_with("last.mp3"));
    assert!(
        events
            .iter()
            .any(|e| e["type"] == "error" && e["message"].as_str().unwrap().contains("missing"))
    );
}
