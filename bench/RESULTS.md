| Download | Size | yt-dlp | cheek | Speed-up |
| --- | ---: | ---: | ---: | ---: |
| Sintel 2K, archive.org (far away, paced per connection) | 296 MiB | 22.7 s | 13.3 s | **1.7x** |
| Sintel 1080p, download.blender.org (direct file) | 1125 MiB | 36.6 s | 30.3 s | **1.2x** |
| Sintel, YouTube 1080p + audio | 180 MiB | 8.3 s | 8.2 s (incl. 2.7 s yt-dlp -J) | **1.0x** |
| Caminandes 3, YouTube 1080p + audio | 25 MiB | 3.4 s | 3.3 s (incl. 2.4 s yt-dlp -J) | **1.0x** |

Windows 11 on a ~300 Mbit/s line, yt-dlp 2026.10.26 with its default single connection, cheek 0.1.0 with 8, best of 2 runs each, same formats on both sides, MD5 checked against Blender's published sum. yt-dlp ran with `--no-check-certificates` for the two non-YouTube hosts, because its Python TLS stack rejects their certificate chains (curl and cheek accept them); that doesn't change its speed.
