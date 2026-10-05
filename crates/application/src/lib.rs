//! Shared use cases for desktop shells. No Tauri, OS credential backend or FFI.
//!
//! [`ApplicationHandle`] is owned once per application, across views/windows.
//! Connections and network sessions stay on Rust workers. Dropping an awaiting
//! future does not undo a committed toggle or cancel an admitted sync job.
#![forbid(unsafe_code)]

pub mod credentials;
pub mod error;
mod lifecycle;
mod local_worker;
pub mod models;
pub mod query;
mod runtime;
pub mod settings;
mod settings_worker;
mod sync_service;
mod sync_worker;

pub use runtime::ApplicationHandle;
pub use sync_service::{SourceSyncResult, SyncBackend, SyncCounts, SyncFailure, SyncReport};
pub use sync_worker::SyncStatus;

#[cfg(test)]
mod test_support;
