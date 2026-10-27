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
