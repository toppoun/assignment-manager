//! Local application actions, independent of network and source synchronization.

use crate::domain::{AssignmentId, CompletionState};
use crate::persistence::{PersistenceError, SqliteStore};

/// Atomically flip local completion and return the state after commit.
///
/// Incomplete becomes Completed and Completed becomes Incomplete. Only completion
/// and updated_at change; source reimports preserve this local state. The store
/// reads and writes under one immediate transaction, sharing the explicit set
/// operation's update contract. Missing IDs return the safe NotFound category;
/// failures contain no entity values or driver causes and never commit a partial change.
///
/// ```
/// use assignment_manager::actions::toggle_assignment_completion;
/// use assignment_manager::domain::{CompletionState, SubmissionTarget};
/// use assignment_manager::persistence::{ManualAssignmentInput, SqliteStore};
///
/// let mut store = SqliteStore::open(":memory:")?;
/// let assignment = store.create_manual_assignment(ManualAssignmentInput {
///     title: "Report".into(),
///     deadline: None,
///     course_id: None,
///     submission: SubmissionTarget::Unknown,
///     submission_url: None,
/// })?;
/// assert_eq!(toggle_assignment_completion(&mut store, &assignment.id)?, CompletionState::Completed);
/// # Ok::<(), assignment_manager::persistence::PersistenceError>(())
/// ```
pub fn toggle_assignment_completion(
    store: &mut SqliteStore,
    id: &AssignmentId,
) -> Result<CompletionState, PersistenceError> {
    store.toggle_completion(id)
}
