//! HTTP client for the Deadlock Mods API (blocking, `ureq`).

mod client;
mod dto;
mod error;
mod query;

pub use client::Client;
pub use dto::{DownloadFile, ModDto};
pub use error::ApiError;
pub use query::{ModPage, ModQuery, SortKey, apply_query};
