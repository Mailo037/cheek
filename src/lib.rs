//! cheek: fast media downloads.
//!
//! Resolving a link (which streams, which URLs) is the part that changes every week, so
//! cheek leaves the big sites to yt-dlp and reads its JSON. What cheek does itself is the
//! part where speed comes from: many ranged requests in parallel, HLS segments in
//! parallel, resumable `.part` files and an ffmpeg merge that copies instead of encoding.

pub mod download;
pub mod events;
pub mod extract;
pub mod hls;
pub mod http;
pub mod media;
pub mod merge;
pub mod run;
pub mod select;
pub mod template;

pub use media::{Media, Protocol, Stream, StreamKind};
pub use run::{Job, run};
