//! Throughput trials with a hard request budget shared by every media stream.
use crate::download::Shared;
use std::time::Duration;

pub struct Guard(Option<tokio::task::JoinHandle<()>>);
impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(h) = &self.0 {
            h.abort();
        }
    }
}

pub fn watch(shared: &Shared) -> Guard {
    if !shared.options.adaptive {
        return Guard(None);
    }
    let (permits, progress) = (shared.permits.clone(), shared.progress.clone());
    let max = shared.options.connections.clamp(1, 64);
    Guard(Some(tokio::spawn(async move {
        let mut capacity = max.min(2);
        let mut held = Vec::new();
        let mut last = progress.snapshot().0;
        let mut pressure = progress.pressure();
        let mut baseline = 0.0;
        let mut trial = false;
        let mut cooldown = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let now = progress.snapshot().0;
            let rate = now.saturating_sub(last) as f64 / 2.0;
            last = now;
            let errors = progress.pressure();
            let target = if errors != pressure {
                trial = false;
                cooldown = 4;
                (capacity / 2).max(1)
            } else if trial && rate < baseline * 1.05 {
                trial = false;
                cooldown = 4;
                capacity.saturating_sub(1).max(1)
            } else if cooldown > 0 {
                cooldown -= 1;
                capacity
            } else if rate > 0.0 && capacity < max {
                baseline = rate;
                trial = true;
                capacity + 1
            } else {
                trial = false;
                capacity
            };
            pressure = errors;
            // Reserved permits lower the cap without interrupting requests already in flight.
            while capacity > target {
                match permits.clone().acquire_owned().await {
                    Ok(p) => {
                        held.push(p);
                        capacity -= 1;
                    }
                    Err(_) => return,
                }
            }
            while capacity < target {
                if let Some(p) = held.pop() {
                    drop(p);
                } else {
                    permits.add_permits(1);
                }
                capacity += 1;
            }
        }
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{download::Options, events::Progress};
    use std::sync::Arc;
    use tokio::sync::Semaphore;

    #[tokio::test(start_paused = true)]
    async fn grows_on_throughput_and_reduces_on_pressure() {
        let s = Shared {
            client: reqwest::Client::new(),
            permits: Arc::new(Semaphore::new(2)),
            progress: Arc::new(Progress::default()),
            options: Options::default(),
        };
        let guard = watch(&s);
        tokio::task::yield_now().await;
        s.progress.add(1000);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(s.permits.available_permits(), 3);
        s.progress.throttled();
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert_eq!(s.permits.available_permits(), 1);
        drop(guard);
        tokio::task::yield_now().await;
    }
}
