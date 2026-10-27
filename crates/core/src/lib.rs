//! idl-core — the download engine behind Internet Downloader.
//!
//! Module map (see Progress.md for the roadmap):
//! - [`types`]      shared data types (requests, status, snapshots)
//! - [`categories`] IDM-style file categories by extension
//! - [`filename`]   filename extraction / sanitising / unique-path helpers
//! - [`probe`]      server capability probe (size, ranges, name, validators)
//! - [`error`]      error type

pub mod categories;
pub mod error;
pub mod filename;
pub mod net;
pub mod probe;
pub mod types;

pub use error::{Error, Result};
