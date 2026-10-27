# Working on cheek

Notes for anyone (human or agent) changing the code. The README is for people who use cheek.

## Layout

- `src/download.rs`: ranged downloads. Chunk sizes adapt so that every connection stays busy to the end. Failed ranges retry with backoff, without counting their bytes twice.
- `src/hls.rs`: master and media playlists, init segments, parallel segments written in order, resume notes for fragments.
- `src/extract/`: the generic extractor, the yt-dlp JSON reader and the CineSrc resolver.
- `src/select.rs`: yt-dlp's `-f` format selection on an info dict that is already resolved.
- `src/merge.rs`: ffmpeg calls that copy streams, embed covers and tags; they only re-encode when you ask for a different audio format.

## Build and check

```bash
cargo build --release
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

CI runs the last three on every push.

### Android

```bash
cargo ndk -t arm64-v8a -t armeabi-v7a -t x86_64 --platform 26 build --release
```

TLS uses rustls with bundled Mozilla roots, so the binary runs as a plain process inside an app, with no JNI. Hamstr packs it as `libcheek.so` and runs it from the app's native library folder; point Hamstr's `cheek.dir` in `local.properties` at this checkout to test a local build there.

## Releasing

Bump `version` in `Cargo.toml`, then push a tag:

```bash
git tag v0.2.0
git push origin v0.2.0
```

[`release.yml`](.github/workflows/release.yml) builds every binary and publishes them with `SHA256SUMS.txt`. If `.github/release-notes/<tag>.md` exists, it becomes the release text. Hamstr picks up releases with the same command line by itself (same major version, for 0.x the same minor).
