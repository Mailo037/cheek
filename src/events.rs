//! What cheek tells the outside: a progress bar for people, JSON lines for apps.

use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Bytes across every stream of one download; shared by all workers.
#[derive(Default)]
pub struct Progress {
    total: AtomicU64,
    done: AtomicU64,
}

impl Progress {
    pub fn add_total(&self, n: u64) {
        self.total.fetch_add(n, Ordering::Relaxed);
    }
    pub fn set_total_at_least(&self, n: u64) {
        self.total.fetch_max(n, Ordering::Relaxed);
    }
    pub fn add(&self, n: u64) {
        self.done.fetch_add(n, Ordering::Relaxed);
    }
    /// Takes back bytes from a request that failed half way and will be fetched again.
    pub fn rewind(&self, n: u64) {
        self.done.fetch_sub(n, Ordering::Relaxed);
    }
    pub fn snapshot(&self) -> (u64, u64) {
        (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event<'a> {
    Info {
        id: &'a str,
        title: &'a str,
        extractor: &'a str,
        streams: usize,
        size: Option<u64>,
    },
    Progress {
        downloaded: u64,
        total: u64,
        percent: f64,
        speed: u64,
        eta: Option<u64>,
    },
    Stage {
        stage: &'a str,
    },
    Done {
        path: &'a str,
        bytes: u64,
        seconds: f64,
    },
    Error {
        message: &'a str,
    },
}

#[derive(Clone)]
pub struct Reporter {
    json: bool,
    quiet: bool,
    bar: Option<ProgressBar>,
}

impl Reporter {
    pub fn new(json: bool, quiet: bool) -> Self {
        let bar = (!json && !quiet).then(|| {
            let bar = ProgressBar::new(0);
            bar.set_style(
                ProgressStyle::with_template(
                    "{spinner} {bar:32.yellow/black} {bytes:>10}/{total_bytes:<10} {binary_bytes_per_sec:>12}  eta {eta}",
                )
                .unwrap()
                .progress_chars("━━─"),
            );
            bar.enable_steady_tick(Duration::from_millis(120));
            bar.set_draw_target(indicatif::ProgressDrawTarget::hidden());
            bar
        });
        Reporter { json, quiet, bar }
    }

    pub fn emit(&self, e: &Event) {
        if self.json {
            println!("{}", serde_json::to_string(e).unwrap());
            return;
        }
        if self.quiet {
            if let Event::Error { message } = e {
                eprintln!("error: {message}");
            }
            return;
        }
        match e {
            Event::Info {
                title,
                extractor,
                streams,
                size,
                ..
            } => {
                let size = size.map(human).unwrap_or_else(|| "unknown size".into());
                eprintln!(
                    "[{extractor}] {title}  ({streams} stream{}, {size})",
                    if *streams == 1 { "" } else { "s" }
                );
            }
            Event::Progress { downloaded, total, .. } => {
                if let Some(bar) = &self.bar {
                    if bar.is_hidden() {
                        bar.set_draw_target(indicatif::ProgressDrawTarget::stderr());
                    }
                    bar.set_length(*total);
                    bar.set_position(*downloaded);
                }
            }
            Event::Stage { stage } => {
                if let Some(bar) = &self.bar {
                    bar.suspend(|| eprintln!("[cheek] {stage}"));
                } else {
                    eprintln!("[cheek] {stage}");
                }
            }
            Event::Done { path, bytes, seconds } => {
                if let Some(bar) = &self.bar {
                    bar.finish_and_clear();
                }
                let rate = if *seconds > 0.0 { (*bytes as f64 / seconds) as u64 } else { 0 };
                eprintln!("[cheek] {} in {:.1} s ({}/s) -> {path}", human(*bytes), seconds, human(rate));
            }
            Event::Error { message } => {
                if let Some(bar) = &self.bar {
                    bar.abandon();
                }
                eprintln!("error: {message}");
            }
        }
    }

    /// Reports `progress` every quarter second until the returned guard is dropped.
    pub fn watch(&self, progress: Arc<Progress>) -> WatchGuard {
        let me = self.clone();
        let handle = tokio::spawn(async move {
            let mut last = (Instant::now(), progress.snapshot().0);
            let mut speed = 0f64;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let (done, total) = progress.snapshot();
                let now = Instant::now();
                let dt = now.duration_since(last.0).as_secs_f64();
                if dt > 0.0 {
                    let inst = done.saturating_sub(last.1) as f64 / dt;
                    // Smooth over about two seconds so the number doesn't jitter.
                    speed = if speed == 0.0 { inst } else { speed * 0.85 + inst * 0.15 };
                }
                last = (now, done);
                let eta = (speed > 1.0 && total > done).then(|| ((total - done) as f64 / speed) as u64);
                let percent = if total > 0 {
                    (done as f64 * 100.0 / total as f64).min(100.0)
                } else {
                    0.0
                };
                me.emit(&Event::Progress {
                    downloaded: done,
                    total,
                    percent: (percent * 10.0).round() / 10.0,
                    speed: speed as u64,
                    eta,
                });
            }
        });
        WatchGuard(handle)
    }
}

pub struct WatchGuard(tokio::task::JoinHandle<()>);

impl Drop for WatchGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

pub fn human(b: u64) -> String {
    let b = b as f64;
    match b {
        b if b >= 1024f64.powi(3) => format!("{:.2} GiB", b / 1024f64.powi(3)),
        b if b >= 1024f64.powi(2) => format!("{:.1} MiB", b / 1024f64.powi(2)),
        b if b >= 1024.0 => format!("{:.0} KiB", b / 1024.0),
        b => format!("{b} B"),
    }
}
