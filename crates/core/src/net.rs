//! HTTP client construction and request decoration.

use crate::types::RequestInfo;
use crate::Result;
use reqwest::header::{HeaderName, HeaderValue, ACCEPT_ENCODING, COOKIE, REFERER};
use reqwest::{Client, RequestBuilder};
use std::time::Duration;

pub const DEFAULT_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

#[derive(Clone, Debug)]
pub struct ClientOptions {
    /// Explicit proxy URL (http://, https://, socks5://). `None` = use system / env proxy.
    pub proxy: Option<String>,
    pub connect_timeout: Duration,
}

impl Default for ClientOptions {
    fn default() -> Self {
        Self { proxy: None, connect_timeout: Duration::from_secs(20) }
    }
}

/// Build the shared HTTP client. TLS is the OS-native stack (SChannel / Security.framework / OpenSSL).
pub fn build_client(opts: &ClientOptions) -> Result<Client> {
    let mut b = Client::builder()
        .connect_timeout(opts.connect_timeout)
        .redirect(reqwest::redirect::Policy::limited(12))
        .pool_max_idle_per_host(32)
        .tcp_nodelay(true);
    if let Some(p) = &opts.proxy {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    Ok(b.build()?)
}

/// Apply referer / cookies / UA / custom headers / auth to a request.
/// Always asks for `identity` encoding so byte ranges line up with the stored file.
pub fn apply_request(mut rb: RequestBuilder, req: &RequestInfo) -> RequestBuilder {
    rb = rb
        .header(ACCEPT_ENCODING, "identity")
        .header(reqwest::header::USER_AGENT, req.user_agent.as_deref().unwrap_or(DEFAULT_UA));
    if let Some(r) = &req.referer {
        if let Ok(v) = HeaderValue::from_str(r) {
            rb = rb.header(REFERER, v);
        }
    }
    if let Some(c) = &req.cookies {
        if let Ok(v) = HeaderValue::from_str(c) {
            rb = rb.header(COOKIE, v);
        }
    }
    for (k, v) in &req.headers {
        if let (Ok(name), Ok(val)) = (HeaderName::from_bytes(k.as_bytes()), HeaderValue::from_str(v)) {
            // Never let callers override transport-critical headers.
            let n = name.as_str();
            if matches!(n, "range" | "host" | "content-length" | "accept-encoding" | "connection") {
                continue;
            }
            rb = rb.header(name, val);
        }
    }
    if let Some(u) = &req.username {
        rb = rb.basic_auth(u, req.password.as_deref());
    }
    rb
}
