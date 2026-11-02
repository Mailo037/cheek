//! A stream that finished in an earlier run, whose merge then failed, isn't fetched again.

use cheek::download::Options;
use cheek::{Job, Media, Protocol, Stream, StreamKind};

#[tokio::test]
async fn finished_streams_from_an_earlier_run_are_used_as_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let media = Media {
        id: "abc".into(),
        title: "Clip".into(),
        extractor: "Generic".into(),
        webpage_url: String::new(),
        thumbnail: None,
        duration: None,
        uploader: None,
        // Nothing listens there: a fetch would fail.
        streams: vec![Stream {
            url: "http://127.0.0.1:9/clip.mp4".into(),
            ext: "mp4".into(),
            kind: StreamKind::Muxed,
            protocol: Protocol::Http,
            format_id: Some("18".into()),
            size: None,
            codec: None,
            height: None,
            headers: Default::default(),
            chunk_limit: None,
            fragments: Vec::new(),
        }],
        subtitles: Vec::new(),
    };
    let bytes = b"all of the stream".to_vec();
    std::fs::write(dir.path().join("Clip [abc].f18.mp4"), &bytes).unwrap();
    let path = cheek::run(
        cheek::http::download_client(None, 2).unwrap(),
        Job {
            media,
            out_dir: dir.path().to_path_buf(),
            temp_dir: None,
            template: "%(title)s [%(id)s].%(ext)s".into(),
            audio: None,
            merge_format: None,
            max_height: None,
            ffmpeg: None,
            options: Options::default(),
            embed_thumbnail: false,
            embed_metadata: false,
            subtitles: Vec::new(),
            playlist_title: None,
        },
        &cheek::events::Reporter::new(false, true),
    )
    .await
    .unwrap();
    assert_eq!(path.file_name().unwrap(), "Clip [abc].mp4");
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!dir.path().join("Clip [abc].f18.mp4").exists());
}
