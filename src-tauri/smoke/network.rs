//! Synthetic, offline transport for the smoke binary only. Never creates an HTTP client.
use assignment_manager::{
    domain::{Deadline, SubmissionTarget},
    import::{AssignmentCandidate, AssignmentCandidateBatch, ExternalRecordKey, OriginKind},
    observation::{AssignmentObservation, SyncScope},
    persistence::{MoodleCourseResolution, SqliteStore},
    sync::WebClassCredentialProvider,
};
use assignment_manager_application::settings::MOODLE_SCOPE;
use assignment_manager_application::{
    SyncBackend, SyncCounts, SyncFailure,
    credentials::{SecretText, WebClassCredentials},
};
use std::{collections::HashMap, thread, time::Duration};

pub struct SmokeNetwork {
    fail_moodle: bool,
    fail_webclass: bool,
}
impl SmokeNetwork {
    pub fn new(mode: &str) -> Self {
        Self {
            fail_moodle: mode == "failure",
            fail_webclass: mode == "partial" || mode == "failure",
        }
    }
}
impl SyncBackend for SmokeNetwork {
    type Session = ();
    fn moodle(
        &mut self,
        url: SecretText,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        drop(url);
        thread::sleep(Duration::from_secs(2));
        if self.fail_moodle {
            return Err(SyncFailure::NetworkFailed);
        }
        import(store, OriginKind::Moodle)
    }
    fn login(&mut self, credentials: WebClassCredentials) -> Result<(), SyncFailure> {
        drop(credentials);
        Ok(())
    }
    fn webclass(
        &mut self,
        _: &mut (),
        _: &mut impl WebClassCredentialProvider,
        store: &mut SqliteStore,
    ) -> Result<SyncCounts, SyncFailure> {
        thread::sleep(Duration::from_secs(2));
        if self.fail_webclass {
            return Err(SyncFailure::AuthenticationFailed);
        }
        import(store, OriginKind::WebClass)
    }
}

fn import(store: &mut SqliteStore, origin: OriginKind) -> Result<SyncCounts, SyncFailure> {
    let (scope, category, title) = match origin {
        OriginKind::Moodle => (
            MOODLE_SCOPE,
            "同期テスト",
            "Moodleから同期した課題（synthetic）",
        ),
        OriginKind::WebClass => (
            "smoke-synced-webclass",
            "同期テスト",
            "WebClassから同期した課題（synthetic）",
        ),
        _ => return Err(SyncFailure::Unexpected),
    };
    let persistence = |_| SyncFailure::PersistenceFailed;
    let course_scope = if origin == OriginKind::Moodle {
        MOODLE_SCOPE
    } else {
        "default"
    };
    let external_course = if origin == OriginKind::Moodle {
        category
    } else {
        scope
    };
    let course = match store
        .find_course_by_source(origin, course_scope, external_course)
        .map_err(persistence)?
    {
        Some(course) => course,
        None => store
            .create_course_with_source(category, vec![], origin, course_scope, external_course)
            .map_err(persistence)?,
    };
    let key = ExternalRecordKey {
        origin,
        scope: scope.into(),
        id: "smoke-synced-assignment".into(),
    };
    let batch = AssignmentCandidateBatch::try_new(vec![AssignmentCandidate {
        external_key: key.clone(),
        title: title.into(),
        deadline: Some(Deadline::DateOnly(
            "2026-12-31".parse().map_err(|_| SyncFailure::Unexpected)?,
        )),
        submission: SubmissionTarget::Unknown,
        submission_url: None,
    }])
    .map_err(|_| SyncFailure::SourceDataInvalid)?;
    let summary = if origin == OriginKind::Moodle {
        let observation = AssignmentObservation::try_new(
            SyncScope::MoodleFeed(MOODLE_SCOPE.into()),
            vec![key.clone()],
        )
        .map_err(|_| SyncFailure::SourceDataInvalid)?;
        let resolutions = HashMap::from([(key, MoodleCourseResolution::new(course.id, category))]);
        store.import_assignment_batch_with_observation_and_courses(
            &batch,
            &observation,
            &resolutions,
            chrono::Utc::now(),
        )
    } else {
        store.import_assignment_batch(&batch)
    }
    .map_err(persistence)?;
    Ok(summary.into())
}
