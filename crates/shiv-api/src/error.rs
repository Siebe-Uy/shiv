use std::io;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("server returned HTTP {status} for {url}")]
    Http { status: u16, url: String },
    #[error("network error: {0}")]
    Network(String),
    #[error("could not parse response: {0}")]
    Parse(String),
    #[error("file error: {0}")]
    Io(#[from] io::Error),
    #[error("download was empty (0 bytes)")]
    EmptyDownload,
    #[error("download incomplete: received {received} of {expected} bytes")]
    Incomplete { received: u64, expected: u64 },
    #[error("download cancelled")]
    Cancelled,
}
