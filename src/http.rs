//! HTTP clients. Downloads use HTTP/1.1 on purpose: parallel ranges only add bandwidth
//! when they travel over separate connections, which HTTP/2 would fold into one.

use crate::media::Stream;
use reqwest::{Client, RequestBuilder, header};
use std::time::Duration;

pub const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36";

fn builder(user_agent: Option<&str>) -> reqwest::ClientBuilder {
    Client::builder()
        .user_agent(user_agent.unwrap_or(USER_AGENT))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(10))
        .tcp_nodelay(true)
}

/// For pages and playlists.
pub fn page_client(user_agent: Option<&str>) -> reqwest::Result<Client> {
    builder(user_agent).build()
}

/// For media bytes: one TCP connection per request in flight.
pub fn download_client(user_agent: Option<&str>, connections: usize) -> reqwest::Result<Client> {
    builder(user_agent)
        .http1_only()
        .pool_max_idle_per_host(connections.max(1) * 2)
        // Media hosts compress nothing worth it, and gzip would break byte ranges.
        .no_gzip()
        .no_brotli()
        .build()
}

/// A GET for a stream, with the headers its extractor asked for.
pub fn get(client: &Client, stream: &Stream) -> RequestBuilder {
    let mut req = client.get(&stream.url);
    for (k, v) in &stream.headers {
        // reqwest sets these itself; a stale copy from the extractor would only confuse servers.
        if k.eq_ignore_ascii_case("accept-encoding") || k.eq_ignore_ascii_case("range") {
            continue;
        }
        req = req.header(k.as_str(), v.as_str());
    }
    req.header(header::ACCEPT, "*/*")
}

/// Negotiated HTTP/2 for A/B measurements; HTTP/1 remains the compatible default.
pub fn negotiated_download_client(user_agent: Option<&str>, connections: usize) -> reqwest::Result<Client> {
    builder(user_agent)
        .pool_max_idle_per_host(connections.max(1) * 2)
        .no_gzip()
        .no_brotli()
        .build()
}

/// Respect server cooldowns (seconds or HTTP date), with jitter to spread retries.
pub fn retry_delay(headers: Option<&header::HeaderMap>, attempt: u32) -> Duration {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let server = headers
        .and_then(|h| h.get(header::RETRY_AFTER))
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>().ok().map(Duration::from_secs).or_else(|| {
                httpdate::parse_http_date(v)
                    .ok()
                    .map(|t| t.duration_since(std::time::SystemTime::now()).unwrap_or_default())
            })
        });
    let salt = SEQUENCE.fetch_add(1, Ordering::Relaxed).wrapping_mul(6364136223846793005)
        ^ std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos() as u64;
    server
        .unwrap_or_else(|| Duration::from_millis(300 * 2u64.pow(attempt.min(6))))
        .saturating_add(Duration::from_millis(salt % 300))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_seconds_and_dates_are_minimum_delays() {
        let mut h = header::HeaderMap::new();
        h.insert(header::RETRY_AFTER, "7".parse().unwrap());
        assert!(retry_delay(Some(&h), 1) >= Duration::from_secs(7));
        let date = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(30));
        h.insert(header::RETRY_AFTER, date.parse().unwrap());
        assert!(retry_delay(Some(&h), 1) >= Duration::from_secs(28));
        h.insert(header::RETRY_AFTER, "invalid".parse().unwrap());
        let d = retry_delay(Some(&h), 2);
        assert!((Duration::from_millis(1200)..Duration::from_millis(1500)).contains(&d));
    }
}
