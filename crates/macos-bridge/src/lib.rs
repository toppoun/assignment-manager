//! Thin macOS transport boundary. Workers, storage and policy belong to application/core.
//! No handwritten FFI, OS paths, settings, sync or credential exports.

mod dto;
mod error;
mod ports;

pub use dto::*;
pub use error::BridgeError;

use assignment_manager_application::{ApplicationHandle, query::AssignmentQuery};
use std::{path::PathBuf, sync::Arc};

uniffi::setup_scaffolding!();

/// UniFFI owns an Arc of this object; the handle owns the shared Rust worker lifetime.
/// Dropping the last proxy initiates nonblocking cleanup. Prefer awaiting shutdown.
#[derive(uniffi::Object)]
pub struct MacApplication {
    application: ApplicationHandle,
}

#[uniffi::export]
impl MacApplication {
    /// The native shell supplies an absolute database path. Validate worker initialization
    /// before returning a usable proxy; shared ApplicationHandle::open remains unchanged.
    #[uniffi::constructor]
    pub async fn open(database_path: String) -> Result<Arc<Self>, BridgeError> {
        let application = ApplicationHandle::open(Ok(PathBuf::from(database_path)), || {
            Ok((ports::PocSettings, ports::NoCredentials))
        })?;
        if let Err(error) = application
            .query_assignments(AssignmentQuery::default())
            .await
        {
            application.shutdown().await;
            return Err(error.into());
        }
        Ok(Arc::new(Self { application }))
    }

    /// Omitted options use the shared default: incomplete, all courses/deadlines,
    /// deadline sort. Swift never filters or sorts this response.
    #[uniffi::method(default(options = None))]
    pub async fn query_assignments(
        &self,
        options: Option<QueryOptions>,
    ) -> Result<AssignmentListDto, BridgeError> {
        self.application
            .query_assignments(options.map(Into::into).unwrap_or_default())
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Commit acknowledgement only. Refresh is a separate call with its own outcome.
    pub async fn toggle_completion(&self, id: String) -> Result<CompletionDto, BridgeError> {
        self.application
            .toggle_completion(id)
            .await
            .map(Into::into)
            .map_err(Into::into)
    }

    /// Reject new requests across every proxy, drain accepted work, await worker cleanup.
    /// Result permits UniFFI to report an unexpected panic through Swift throws.
    pub async fn shutdown(&self) -> Result<(), BridgeError> {
        self.application.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
