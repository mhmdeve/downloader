use serde::{Deserialize, Serialize};

pub type DownloadId = u64;

/// Everything needed to (re)issue requests for one download.
/// The browser extension fills `referer`, `cookies` and `user_agent` so that
/// authenticated / hot-link-protected downloads work like in the browser.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct RequestInfo {
    pub url: String,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    /// Extra headers (name, value).
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    /// HTTP basic auth.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

impl RequestInfo {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into(), ..Default::default() }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Downloading,
    Paused,
    Completed,
    Error,
}

/// A contiguous byte range `[start, end)` assigned to one connection.
/// `pos` is the next byte to fetch; the segment is done when `pos >= end`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub pos: u64,
    pub end: u64,
}

impl Segment {
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
    pub fn done(&self) -> bool {
        self.pos >= self.end
    }
}
