//! Probe a URL: size, range support, final URL, suggested filename, validators.

use crate::filename::choose_filename;
use crate::net::apply_request;
use crate::types::RequestInfo;
use crate::{Error, Result};
use reqwest::header::{HeaderMap, ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProbeInfo {
    /// URL after redirects.
    pub final_url: String,
    /// Total size in bytes if the server told us.
    pub size: Option<u64>,
    /// True if the server honours `Range` requests (multi-connection + resume possible).
    pub accepts_ranges: bool,
    pub filename: String,
    pub content_type: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

/// Parse `Content-Range: bytes 0-0/12345` or `bytes */12345` and return the total.
pub fn parse_content_range_total(v: &str) -> Option<u64> {
    let (_, total) = v.rsplit_once('/')?;
    let total = total.trim();
    if total == "*" {
        None
    } else {
        total.parse().ok()
    }
}

/// Decode header bytes: UTF-8 if valid, else Latin-1.
fn header_text(h: &HeaderMap, name: reqwest::header::HeaderName) -> Option<String> {
    let v = h.get(name)?;
    let b = v.as_bytes();
    Some(match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    })
}

pub async fn probe(client: &Client, req: &RequestInfo) -> Result<ProbeInfo> {
    let url = Url::parse(&req.url).map_err(|e| Error::BadUrl(format!("{}: {e}", req.url)))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(Error::BadUrl(format!("unsupported scheme '{}'", url.scheme())));
    }

    // GET with a 0-0 range is more widely supported than HEAD and tells us about range support too.
    let resp = apply_request(client.get(url.clone()), req).header(RANGE, "bytes=0-0").send().await?;
    let status = resp.status();
    let h = resp.headers().clone();
    let final_url = resp.url().clone();
    drop(resp); // we only wanted the headers

    let (size, accepts_ranges) = match status {
        StatusCode::PARTIAL_CONTENT => {
            let total = header_text(&h, CONTENT_RANGE).and_then(|v| parse_content_range_total(&v));
            (total, total.is_some())
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            // Empty file: `bytes */0`.
            let total = header_text(&h, CONTENT_RANGE).and_then(|v| parse_content_range_total(&v));
            (total, total.is_some())
        }
        s if s.is_success() => {
            let len = header_text(&h, CONTENT_LENGTH).and_then(|v| v.trim().parse().ok());
            // Server ignored Range => single connection only, even if it advertises bytes.
            let _advertised = header_text(&h, ACCEPT_RANGES).map(|v| v.eq_ignore_ascii_case("bytes"));
            (len, false)
        }
        s => return Err(Error::BadStatus(s.as_u16())),
    };

    let content_type = header_text(&h, CONTENT_TYPE).map(|v| v.split(';').next().unwrap_or("").trim().to_ascii_lowercase());
    let cd = header_text(&h, CONTENT_DISPOSITION);
    let filename = choose_filename(&final_url, cd.as_deref(), content_type.as_deref());

    Ok(ProbeInfo {
        final_url: final_url.to_string(),
        size,
        accepts_ranges,
        filename,
        content_type,
        etag: header_text(&h, ETAG),
        last_modified: header_text(&h, LAST_MODIFIED),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        assert_eq!(parse_content_range_total("bytes 0-0/12345"), Some(12345));
        assert_eq!(parse_content_range_total("bytes */777"), Some(777));
        assert_eq!(parse_content_range_total("bytes 0-0/*"), None);
        assert_eq!(parse_content_range_total("garbage"), None);
    }
}
