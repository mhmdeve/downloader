use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("network error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("server returned HTTP {0}")]
    BadStatus(u16),
    #[error("invalid URL: {0}")]
    BadUrl(String),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("the remote file changed since the download started")]
    RemoteChanged,
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Whether retrying the same request might succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Error::Http(e) => !e.is_builder() && !e.is_redirect(),
            Error::BadStatus(c) => matches!(*c, 408 | 425 | 429 | 500 | 502 | 503 | 504),
            Error::Io(_) => false,
            _ => false,
        }
    }
}
