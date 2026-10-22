//! Explicit offline HTTPS runtime; no arbitrary policy/provider/backend input.
use crate::{PocState, SystemClock, WebError, auth, keys::MemoryKeyProvider, poc::PocDatabase};
pub use assignment_manager::webclass_auth::synthetic::{
    MOODLE_URL, Scenario, SyntheticUniversity, UNIVERSITY_ID, UNIVERSITY_PASSWORD,
};
use assignment_manager_application::SecuredCoreSyncBackend;
use std::sync::Arc;

/// Keep the returned university alive until the state/server has drained.
/// Only memory keys are accepted. This is never a normal Mac startup fallback.
pub async fn open(
    database: &PocDatabase,
    provider: Arc<MemoryKeyProvider>,
    scenario: Scenario,
) -> Result<(PocState, SyntheticUniversity), WebError> {
    let university = SyntheticUniversity::start(scenario);
    let state = PocState::open_with_backend(
        database,
        SystemClock,
        auth::SystemAuthClock,
        provider,
        SecuredCoreSyncBackend::new(university.policy()),
    )
    .await?;
    Ok((state, university))
}
