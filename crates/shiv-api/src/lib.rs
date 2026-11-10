//! HTTP client for the Deadlock Mods API (blocking, `ureq`).

mod client;
mod dto;
mod error;
mod gamebanana;
mod query;

pub use client::Client;
pub use dto::{DonationLink, DownloadFile, ModDto, ModMetadata, is_donation_url};
pub use error::ApiError;
pub use gamebanana::{ChangelogEntry, Comment, Comments, GameBananaClient, ModFile, Page};
pub use query::{ModPage, ModQuery, SortKey, apply_query};
