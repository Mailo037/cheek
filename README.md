<p align="center">
  <img src="docs/hero.png" alt="cheek: fast media downloads in Rust" width="100%">
</p>

<p align="center">
  <a href="https://github.com/Mailo037/cheek/releases/latest"><img src="https://img.shields.io/github/v/release/Mailo037/cheek?style=flat-square&color=FF9A3C&labelColor=1E1E1E&label=release" alt="Latest release"></a>
  <a href="https://github.com/Mailo037/cheek/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/Mailo037/cheek/ci.yml?branch=main&style=flat-square&labelColor=1E1E1E&label=ci" alt="CI"></a>
  <img src="https://img.shields.io/badge/linux%20·%20windows%20·%20macos%20·%20android-EEEEEE?style=flat-square&labelColor=1E1E1E&label=runs%20on" alt="Platforms">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-Unlicense-AAAAAA?style=flat-square&labelColor=1E1E1E" alt="Unlicense"></a>
</p>

**cheek** downloads media fast. It is a single Rust binary of about 4 MB. It splits each file into byte ranges and fetches them over many connections at once. HLS segments also download in parallel. Interrupted downloads continue where they stopped, and video and audio are merged with ffmpeg without re-encoding. It is the download engine of [Hamstr](https://github.com/Mailo037/Hamstr) and works on its own on Linux, Windows, macOS and Android.

## Where the speed comes from, and where it doesn't

A download has two halves. **Resolving** works out which streams a link has and where they live. **Fetching** moves the bytes.

- **Fetching is where cheek wins.** A single HTTP connection rarely fills a fast line: TCP ramps up slowly, and many CDNs pace each connection. Eight connections that each pull their own range fill the line instead.
- **Resolving stays with [yt-dlp](https://github.com/yt-dlp/yt-dlp).** It has extractors for more than 1,800 sites, and maintainers fix them every week as the sites change. A rewrite would fall behind within days. So for YouTube, Instagram, TikTok and the like, cheek reads yt-dlp's JSON and takes over from there.
- **Direct links, HLS playlists and ordinary web pages** (`og:video`, `<video>`, JSON-LD) are resolved by cheek alone, with no Python involved.

## Benchmarks

<!-- bench:start -->
| Download | Size | yt-dlp | cheek | Speed-up |
| --- | ---: | ---: | ---: | ---: |
| Sintel 2K, archive.org (far away, paced per connection) | 296 MiB | 22.7 s | 13.3 s | **1.7x** |
| Sintel 1080p, download.blender.org (direct file) | 1125 MiB | 36.6 s | 30.3 s | **1.2x** |
| Sintel, YouTube 1080p + audio | 180 MiB | 8.3 s | 8.2 s (incl. 2.7 s yt-dlp -J) | **1.0x** |
| Caminandes 3, YouTube 1080p + audio | 25 MiB | 3.4 s | 3.3 s (incl. 2.4 s yt-dlp -J) | **1.0x** |

Windows 11 on a ~300 Mbit/s line, yt-dlp 2026.10.26 with its default single connection, cheek 0.1.0 with 8, best of 2 runs each, same formats on both sides, MD5 checked against Blender's published sum. yt-dlp ran with `--no-check-certificates` for the two non-YouTube hosts, because its Python TLS stack rejects their certificate chains (curl and cheek accept them); that doesn't change its speed.

**What this means:** cheek wins where one connection can't fill the line, for example a server far away or a host that limits each connection. Archive.org, many CDNs and mobile networks behave like that. On a fast line close to a fast server (YouTube from Europe here), a single connection already runs at full speed, and both tools are as fast as the line allows.
<!-- bench:end -->

Run them yourself with `python bench/bench.py --yt-dlp yt-dlp --ffmpeg ffmpeg --cheek target/release/cheek`. The YouTube rows include the time `yt-dlp -J` needs to resolve the link.

## Use

```bash
# A direct file, an HLS playlist or a page with a video on it
cheek https://example.com/talk.mp4

# Any site yt-dlp knows; cheek calls yt-dlp to resolve the link when it's installed
cheek "https://www.youtube.com/watch?v=SkVqJ1SGeL0" -f "bv*[height<=1080]+ba/b"

# Or hand over yt-dlp's JSON yourself
yt-dlp -J -f "bv*+ba/b" URL | cheek --info-json -

# Resolve once, then pick a quality from the same JSON without starting yt-dlp again
yt-dlp -J URL > info.json
cheek --info-json info.json -f "bv*[height<=720]+ba/b[height<=720]"

# Sound only, without re-encoding when the source is already Opus or AAC
cheek -x URL
cheek -x --audio-format mp3 URL

# Parallel fragments (HLS or a static DASH fragment list resolved by yt-dlp)
cheek URL -N 8

# Smaller byte chunks for direct files; connections are shared by video and audio
cheek https://example.com/talk.mp4 -N 8 --chunk-size 1M
```

| Option | Default | |
| --- | --- | --- |
| `-N, --connections` | `8` | Requests in flight at once |
| `--chunk-size` | `8M` | Largest byte range per request; capped by the host's own limit |
| `-o, --output` | `%(title)s [%(id)s].%(ext)s` | yt-dlp's template syntax (`title`, `id`, `ext`, `extractor`, `uploader`, `.NB` to cut) |
| `-P, --paths` | `.` | Folder for finished files |
| `--temp-dir` | output folder | Where `.part` files wait |
| `-f, --format` | `bv*+ba/b` | yt-dlp format selector. Passed to yt-dlp when it resolves; with `--info-json` cheek picks from the JSON's formats itself, by yt-dlp's rules |
| `--resolver` | `auto` | `auto`: cheek first, yt-dlp for the big sites; `native` or `yt-dlp` to force one |
| `-x`, `--audio-format` | `best` | Keep only the sound: `best`, `opus`, `m4a`, `mp3`, `flac`, `wav` |
| `--merge-output-format` | picked like yt-dlp | `mp4`, `mkv` or `webm` when video and audio are joined |
| `--embed-thumbnail` | | Write the thumbnail into the file as its cover (mp4, m4a, mkv, mp3, opus, ogg, flac) |
| `--embed-metadata` | | Write title, artist and link into every file, even one that needed no ffmpeg step |
| `--json` | | One JSON event per line on stdout, for apps |
| `-J, --dump-json` | | Print what would be downloaded and stop |

Parallel downloading is automatic: direct files use byte ranges when the server supports them; HLS and yt-dlp's `http_dash_segments` formats use the supplied fragments. The source determines their duration (for example, 2–6 seconds); `--chunk-size` controls byte ranges, not fragment duration. Initialization fragments are included and all fragments are joined in order without re-encoding. `-N` limits requests across video and audio together; `-N 1` downloads sequentially. More connections help when individual requests are throttled, but cannot exceed your connection or the server's bandwidth.

Static, unencrypted DASH fragment lists support absolute URLs and paths relative to `fragment_base_url`. Live DASH, encrypted fragments and DASH byte-range fragments are rejected. HLS encryption and byte-range playlists remain unsupported.

Press Ctrl+C and the `.part` file stays. Ranged HTTP downloads keep a small `.cheek` note of completed chunks, and HLS and DASH fragment downloads one of the fragments already written; both resume on the next run, even when a fresh resolve hands out new tokens for the same fragments.

### Cineby / CineSrc (desktop)

CineSrc identifies movies and episodes by TMDB ID. Cheek accepts Cineby movie
links, CineSrc embed links, or a short ID reference:

```bash
cheek cinesrc:movie:TMDB_ID -J
cheek cinesrc:tv:TMDB_ID:SEASON:EPISODE -J
cheek "https://cineby.rocks/movie/TMDB_ID"
cheek "https://cineby.rocks/watch/movie/TMDB_ID"
cheek "https://cinesrc.st/embed/tv/TMDB_ID?s=1&e=2"
```

Replace `TMDB_ID` with the numeric ID. `cineby:` is also accepted as an alias for
`cinesrc:`. TV references require a season and episode; season 0 supports specials.
A bare number is ambiguous between movies and series and is not accepted.

The documented CineSrc interface is a JavaScript player, not a direct media API.
Cheek opens that player in a temporary headless Chrome/Chromium or Edge session,
observes its HLS request and verifies that the playlist works outside the browser.
It then closes the browser and downloads through Cheek's normal HLS engine.
Browser discovery is automatic on desktop; set `CHEEK_BROWSER` to an executable
path if necessary. Your regular browser profile is not used.

Resolution times out after 90 seconds when the player cannot supply a stream.
Availability depends on CineSrc and its selected server. Encrypted HLS remains
unsupported. The standalone resolver needs a desktop browser. On Android, Hamstr
resolves these IDs through the system WebView and hands Cheek the resulting media
JSON through `--info-json`; no desktop browser is needed in the app.
`--resolver yt-dlp` explicitly bypasses the standalone resolver.

### JSON events

With `--json`, every line on stdout is one event:

```json
{"type":"info","id":"SkVqJ1SGeL0","title":"Caminandes 3: Llamigos","extractor":"Youtube","streams":2,"size":26214400}
{"type":"progress","downloaded":10485760,"total":26214400,"percent":40.0,"speed":31457280,"eta":1}
{"type":"stage","stage":"merging"}
{"type":"done","path":"Caminandes 3： Llamigos [SkVqJ1SGeL0].mp4","bytes":26214400,"seconds":1.2}
```

`stage` is one of `merging`, `extracting audio`, `remuxing`, `tagging` (only tags and cover, no other ffmpeg step) or `already downloaded`; it arrives once the bytes are in and ffmpeg takes over. When something fails, the last line is `{"type":"error","message":"..."}` and the exit code is 1. An interruption exits with 130.

## Install

Download a binary from the [latest release](https://github.com/Mailo037/cheek/releases/latest):

| File | For |
| --- | --- |
| `cheek-x86_64-linux`, `cheek-aarch64-linux` | Linux (static, musl) |
| `cheek-x86_64-windows.exe` | Windows |
| `cheek-aarch64-macos`, `cheek-x86_64-macos` | macOS |
| `cheek-android-arm64-v8a`, `-armeabi-v7a`, `-x86_64` | Android apps that ship it as `libcheek.so` |

Or install it with Cargo:

```bash
cargo install --git https://github.com/Mailo037/cheek
```

ffmpeg is only needed for merging, audio extraction and HLS remuxing; a single-file download works without it. yt-dlp is only needed for the sites cheek doesn't resolve by itself.

## How it works

```
 link ─┬─ direct file / HLS / web page ──► cheek's own extractor ─┐
       └─ YouTube, Instagram, TikTok ... ─► yt-dlp -J (JSON) ─────┤
                                                                  ▼
                               streams (video, audio) with URLs and headers
                                                                  │
          probe: size + range support ◄───────────────────────────┤
                                                                  ▼
     ranges ──► N workers ──► write at offset ──► .part + .cheek (resume notes)
     HLS    ──► segments in parallel ──► written in order
                                                                  │
                                         ffmpeg -c copy (merge / remux / audio)
                                                                  ▼
                                                            finished file
```

## License

[Unlicense](LICENSE), public domain, like yt-dlp.
