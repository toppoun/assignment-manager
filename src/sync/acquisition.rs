//! Fetch and validate source data before passing it to a consumer.

use std::{
    collections::{HashMap, HashSet},
    fmt,
};

use chrono::{DateTime, Utc};

use crate::{
    import::{
        AssignmentCandidateBatch, DEFAULT_COURSE_SOURCE_SCOPE, ExternalRecordKey, OriginKind,
    },
    moodle::{MoodleEventKind, classify_event, map_events, parse_calendar, single_course_category},
    moodle_http::{MoodleClient, MoodleDiagnostic, MoodleFeed},
    observation::{
        AssignmentObservation, ObservationValidationError, SyncScope, WebClassCourseListObservation,
    },
    webclass::{
        WebClassCourse, WebClassMapError, WebClassParseError, map_contents_for_group,
        parse_contents, parse_courses,
    },
    webclass_auth::{HoseiAuthError, HoseiAuthenticator, HoseiCredentials},
    webclass_http::{WebClassClient, WebClassHttpError},
};

use super::{
    MoodleSyncError, WebClassCredentialProvider, WebClassReauthSyncError, WebClassSyncError,
};

/// A source course identity and display label, independent of a local or remote page ID.
#[derive(Clone, PartialEq, Eq)]
pub struct SourceCourse {
    pub external_key: ExternalRecordKey,
    pub name: String,
}

impl fmt::Debug for SourceCourse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SourceCourse").finish_non_exhaustive()
    }
}

/// Validated candidates and positive observations for one feed or group.
/// An empty batch can still contain observations. Absence does not imply deletion.
pub struct AcquiredAssignments {
    batch: AssignmentCandidateBatch,
    observation: AssignmentObservation,
    observed_at: DateTime<Utc>,
}

impl AcquiredAssignments {
    pub(crate) fn try_new(
        batch: AssignmentCandidateBatch,
        observation: AssignmentObservation,
    ) -> Result<Self, ObservationValidationError> {
        observation.validate_candidates(&batch)?;
        Ok(Self {
            batch,
            observation,
            observed_at: Utc::now(),
        })
    }

    pub fn batch(&self) -> &AssignmentCandidateBatch {
        &self.batch
    }

    pub fn observation(&self) -> &AssignmentObservation {
        &self.observation
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

impl fmt::Debug for AcquiredAssignments {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcquiredAssignments")
            .field("candidate_count", &self.batch.as_slice().len())
            .field("observation", &self.observation)
            .finish_non_exhaustive()
    }
}

/// A Moodle feed result. Course metadata exists only for candidates with a
/// usable single category from an explicitly CourseRelated export.
pub struct MoodleAcquisition {
    pub(crate) assignments: AcquiredAssignments,
    pub(crate) courses: HashMap<ExternalRecordKey, SourceCourse>,
    pub(crate) event_kinds: HashMap<ExternalRecordKey, MoodleEventKind>,
}

impl fmt::Debug for MoodleAcquisition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoodleAcquisition")
            .field("assignments", &self.assignments)
            .field("course_link_count", &self.courses.len())
            .finish_non_exhaustive()
    }
}

impl MoodleAcquisition {
    pub fn assignments(&self) -> &AcquiredAssignments {
        &self.assignments
    }

    pub fn course_for(&self, assignment: &ExternalRecordKey) -> Option<&SourceCourse> {
        self.courses.get(assignment)
    }

    pub fn event_kind_for(&self, assignment: &ExternalRecordKey) -> Option<MoodleEventKind> {
        self.event_kinds.get(assignment).copied()
    }
}

/// The existing failure category plus a fixed diagnostic for the read-only check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoodleAcquisitionError {
    pub kind: MoodleSyncError,
    pub diagnostic: MoodleDiagnostic,
}

impl fmt::Display for MoodleAcquisitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.kind.fmt(f)
    }
}

impl std::error::Error for MoodleAcquisitionError {}

fn acquired_response_error(kind: MoodleSyncError, code: &'static str) -> MoodleAcquisitionError {
    MoodleAcquisitionError {
        kind,
        diagnostic: MoodleDiagnostic::new(code, Some(reqwest::StatusCode::OK)),
    }
}

/// Fetch one Moodle feed and return its normalized data without a store.
/// The result contains the feed scope, never the calendar URL.
pub fn acquire_moodle(
    client: &MoodleClient,
    feed: &MoodleFeed,
) -> Result<MoodleAcquisition, MoodleSyncError> {
    acquire_moodle_with_diagnostic(client, feed).map_err(|error| error.kind)
}

/// Fetch once using the same acquisition path, retaining only fixed diagnostic values.
pub fn acquire_moodle_with_diagnostic(
    client: &MoodleClient,
    feed: &MoodleFeed,
) -> Result<MoodleAcquisition, MoodleAcquisitionError> {
    let mut courses = HashMap::new();
    let mut event_kinds = HashMap::new();
    let assignments = visit_moodle(client, feed, |key, kind, course| {
        if let Some(course) = course {
            courses.insert(key.clone(), course);
        }
        event_kinds.insert(key, kind);
        Ok(())
    })?;
    Ok(MoodleAcquisition {
        assignments,
        courses,
        event_kinds,
    })
}

// Deliver courses in event order, after validation. The SQLite consumer can
// retain its original course commits and budget checkpoints in this order.
pub(super) fn visit_moodle(
    client: &MoodleClient,
    feed: &MoodleFeed,
    mut visit_candidate: impl FnMut(
        ExternalRecordKey,
        MoodleEventKind,
        Option<SourceCourse>,
    ) -> Result<(), MoodleSyncError>,
) -> Result<AcquiredAssignments, MoodleAcquisitionError> {
    let raw = client
        .fetch_calendar_with_diagnostic(feed)
        .map_err(|(_, diagnostic)| MoodleAcquisitionError {
            kind: MoodleSyncError::Fetch,
            diagnostic,
        })?;
    let events = parse_calendar(&raw)
        .map_err(|_| acquired_response_error(MoodleSyncError::Parse, "ics_parse"))?;
    let limit = acquired_response_error(MoodleSyncError::Validation, "candidate_limit");
    let validation = acquired_response_error(MoodleSyncError::Validation, "candidate_validation");
    client.check_sync_records(events.len()).map_err(|_| limit)?;
    let key_for = |id: &str| ExternalRecordKey {
        origin: OriginKind::Moodle,
        scope: feed.scope().to_owned(),
        id: id.to_owned(),
    };
    let observation = AssignmentObservation::try_new(
        SyncScope::MoodleFeed(feed.scope().to_owned()),
        events.iter().map(|event| key_for(&event.uid)).collect(),
    )
    .map_err(|error| match error {
        ObservationValidationError::DuplicateIdentity => {
            acquired_response_error(MoodleSyncError::Validation, "moodle_uid_duplicate")
        }
        _ => validation,
    })?;
    let batch = AssignmentCandidateBatch::try_new(map_events(&events, feed.scope()))
        .map_err(|_| validation)?;
    let assignments = AcquiredAssignments::try_new(batch, observation).map_err(|_| validation)?;
    let candidate_keys: HashSet<_> = assignments
        .batch()
        .as_slice()
        .iter()
        .map(|candidate| &candidate.external_key)
        .collect();
    for event in &events {
        client.check_sync_records(0).map_err(|_| {
            acquired_response_error(MoodleSyncError::Fetch, "course_processing_limit")
        })?;
        let key = key_for(&event.uid);
        if !candidate_keys.contains(&key) {
            continue;
        }
        let course = single_course_category(event, feed).map(|category| SourceCourse {
            external_key: key_for(category),
            name: category.to_owned(),
        });
        visit_candidate(key, classify_event(event), course)
            .map_err(|kind| acquired_response_error(kind, "course_processing"))?;
    }
    client
        .check_sync_records(assignments.batch().as_slice().len())
        .map_err(|_| limit)?;
    Ok(assignments)
}

/// Validated WebClass course discovery. Successful discovery says nothing about
/// the outcome or completeness of any individual contents request.
#[derive(Debug)]
pub struct AcquiredWebClassCourses {
    pub(crate) courses: Vec<SourceCourse>,
    records: Vec<WebClassCourse>,
    pub(crate) observation: WebClassCourseListObservation,
    pub(crate) observed_at: DateTime<Utc>,
}

impl AcquiredWebClassCourses {
    pub(crate) fn from_records(
        records: Vec<WebClassCourse>,
    ) -> Result<Self, ObservationValidationError> {
        let observation = WebClassCourseListObservation::try_new(
            records
                .iter()
                .map(|course| course.group_id.clone())
                .collect(),
        )?;
        let courses = records
            .iter()
            .map(|course| SourceCourse {
                external_key: ExternalRecordKey {
                    origin: OriginKind::WebClass,
                    scope: DEFAULT_COURSE_SOURCE_SCOPE.to_owned(),
                    id: course.group_id.clone(),
                },
                name: course.group_name.clone(),
            })
            .collect();
        Ok(Self {
            courses,
            records,
            observation,
            observed_at: Utc::now(),
        })
    }

    pub fn courses(&self) -> &[SourceCourse] {
        &self.courses
    }

    /// Validated, source-native values. In particular, timetable values and
    /// children have not been interpreted.
    pub fn records(&self) -> &[WebClassCourse] {
        &self.records
    }

    pub fn observation(&self) -> &WebClassCourseListObservation {
        &self.observation
    }

    pub fn observed_at(&self) -> DateTime<Utc> {
        self.observed_at
    }
}

/// Redacted diagnostics for a list-only WebClass acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebClassCourseListError {
    Authentication(HoseiAuthError),
    Fetch(WebClassHttpError),
    Parse(WebClassParseError),
    Limit,
    Validation(ObservationValidationError),
}

impl From<HoseiAuthError> for WebClassCourseListError {
    fn from(error: HoseiAuthError) -> Self {
        Self::Authentication(error)
    }
}

impl From<WebClassCourseListError> for WebClassSyncError {
    fn from(error: WebClassCourseListError) -> Self {
        match error {
            WebClassCourseListError::Authentication(_) => Self::Fetch,
            WebClassCourseListError::Fetch(error) => webclass_fetch_error(error),
            WebClassCourseListError::Parse(_) => Self::Parse,
            WebClassCourseListError::Limit | WebClassCourseListError::Validation(_) => {
                Self::CourseValidation
            }
        }
    }
}

impl fmt::Display for WebClassCourseListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Authentication(error) => error.fmt(f),
            Self::Fetch(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
            Self::Limit => f.write_str("WebClass course list exceeded acquisition limits"),
            Self::Validation(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for WebClassCourseListError {}

/// Use the new session's course-list GET for both authentication verification
/// and acquisition. Does not fetch contents, persist state or retry a failed login.
pub fn login_and_acquire_webclass_courses(
    authenticator: &HoseiAuthenticator,
    credentials: HoseiCredentials<'_>,
) -> Result<AcquiredWebClassCourses, WebClassCourseListError> {
    authenticator.login_with_course_check(credentials, |client| acquire_webclass_courses(&client))
}

/// One pass over the course list in response order. On failure, earlier groups
/// (including valid empty groups) remain available, and traversal stops.
#[derive(Debug)]
pub struct WebClassAcquisitionAttempt {
    pub(crate) course_list: Option<AcquiredWebClassCourses>,
    pub(crate) groups: Vec<AcquiredAssignments>,
    pub(crate) result: Result<(), WebClassSyncError>,
}

impl WebClassAcquisitionAttempt {
    pub fn course_list(&self) -> Option<&AcquiredWebClassCourses> {
        self.course_list.as_ref()
    }

    /// Each batch's observation identifies its exact WebClass group.
    pub fn groups(&self) -> &[AcquiredAssignments] {
        &self.groups
    }

    pub fn result(&self) -> Result<(), WebClassSyncError> {
        self.result
    }
}

/// Ordered attempts, including any validated groups from before auth expiry.
/// Across attempts a group or key may be observed again; use its external key.
#[derive(Debug)]
pub struct WebClassAcquisition {
    attempts: Vec<WebClassAcquisitionAttempt>,
    result: Result<(), WebClassReauthSyncError>,
}

impl WebClassAcquisition {
    pub(crate) fn new(
        attempts: Vec<WebClassAcquisitionAttempt>,
        result: Result<(), WebClassReauthSyncError>,
    ) -> Self {
        Self { attempts, result }
    }

    pub fn attempts(&self) -> &[WebClassAcquisitionAttempt] {
        &self.attempts
    }

    pub fn result(&self) -> Result<(), WebClassReauthSyncError> {
        self.result
    }
}

/// Fetch and validate one exact WebClass contents group with an existing session.
pub fn acquire_webclass_group(
    client: &WebClassClient,
    group_id: &str,
) -> Result<AcquiredAssignments, WebClassSyncError> {
    client
        .check_sync_group()
        .map_err(|_| WebClassSyncError::Fetch)?;
    let raw = client
        .fetch_contents_json(group_id)
        .map_err(webclass_fetch_error)?;
    let contents = parse_contents(&raw).map_err(|_| WebClassSyncError::Parse)?;
    client
        .check_sync_records(contents.len())
        .map_err(|_| WebClassSyncError::Validation)?;
    let observation = AssignmentObservation::try_new(
        SyncScope::WebClassContents(group_id.to_owned()),
        contents
            .iter()
            .map(|content| ExternalRecordKey {
                origin: OriginKind::WebClass,
                // Validate the actual response group even for filtered records.
                scope: content.group_id.clone(),
                id: content.contents_id.clone(),
            })
            .collect(),
    )
    .map_err(|_| WebClassSyncError::Validation)?;
    let batch = map_contents_for_group(&contents, group_id).map_err(|error| match error {
        WebClassMapError::InvalidEndDate => WebClassSyncError::Map,
        WebClassMapError::Validation(_) => WebClassSyncError::Validation,
    })?;
    let assignments = AcquiredAssignments::try_new(batch, observation)
        .map_err(|_| WebClassSyncError::Validation)?;
    client
        .check_sync_records(assignments.batch().as_slice().len())
        .map_err(|_| WebClassSyncError::Validation)?;
    Ok(assignments)
}

fn webclass_fetch_error(error: WebClassHttpError) -> WebClassSyncError {
    match error {
        WebClassHttpError::AuthExpired => WebClassSyncError::AuthExpired,
        _ => WebClassSyncError::Fetch,
    }
}

pub(super) enum WebClassEvent<'a> {
    Course(&'a SourceCourse),
    Assignments(AcquiredAssignments),
}

pub(super) fn acquire_webclass_courses(
    client: &WebClassClient,
) -> Result<AcquiredWebClassCourses, WebClassCourseListError> {
    let raw = client
        .fetch_courses_json()
        .map_err(WebClassCourseListError::Fetch)?;
    let records = parse_courses(&raw).map_err(WebClassCourseListError::Parse)?;
    client
        .check_sync_courses(records.len())
        .map_err(|_| WebClassCourseListError::Limit)?;
    AcquiredWebClassCourses::from_records(records).map_err(WebClassCourseListError::Validation)
}

pub(super) fn visit_webclass_groups(
    client: &WebClassClient,
    courses: &[SourceCourse],
    mut visit: impl FnMut(WebClassEvent<'_>) -> Result<(), WebClassSyncError>,
) -> Result<(), WebClassSyncError> {
    for course in courses {
        client
            .check_sync_courses(0)
            .map_err(|_| WebClassSyncError::Fetch)?;
        visit(WebClassEvent::Course(course))?;
        let assignments = acquire_webclass_group(client, &course.external_key.id)?;
        visit(WebClassEvent::Assignments(assignments))?;
    }
    Ok(())
}

/// Fetch WebClass discovery and groups without a store or reauthentication.
pub fn acquire_webclass_once(client: &WebClassClient) -> WebClassAcquisitionAttempt {
    let course_list = match acquire_webclass_courses(client) {
        Ok(list) => list,
        Err(error) => {
            return WebClassAcquisitionAttempt {
                course_list: None,
                groups: Vec::new(),
                result: Err(error.into()),
            };
        }
    };
    let mut groups = Vec::new();
    let result = visit_webclass_groups(client, &course_list.courses, |event| {
        if let WebClassEvent::Assignments(assignments) = event {
            groups.push(assignments);
        }
        Ok(())
    });
    WebClassAcquisitionAttempt {
        course_list: Some(course_list),
        groups,
        result,
    }
}

/// Fetch WebClass data with the same single recovery attempt as SQLite sync.
/// Successful batches are retained per attempt even when recovery fails.
pub fn acquire_webclass_with_reauth(
    client: &mut WebClassClient,
    authenticator: &HoseiAuthenticator,
    credential_provider: &mut impl WebClassCredentialProvider,
) -> WebClassAcquisition {
    let mut attempts = Vec::new();
    let result = run_with_webclass_reauth(client, authenticator, credential_provider, |client| {
        let attempt = acquire_webclass_once(client);
        let result = attempt.result;
        attempts.push(attempt);
        result
    });
    WebClassAcquisition::new(attempts, result)
}

pub(super) fn run_with_webclass_reauth<T>(
    client: &mut WebClassClient,
    authenticator: &HoseiAuthenticator,
    credential_provider: &mut impl WebClassCredentialProvider,
    mut run: impl FnMut(&WebClassClient) -> Result<T, WebClassSyncError>,
) -> Result<T, WebClassReauthSyncError> {
    match run(client) {
        Ok(value) => return Ok(value),
        Err(WebClassSyncError::AuthExpired) => {}
        Err(error) => return Err(WebClassReauthSyncError::Sync(error)),
    }
    let new_client = credential_provider
        .with_credentials(|credentials| authenticator.login(credentials))
        .ok_or(WebClassReauthSyncError::CredentialsUnavailable)?
        .map_err(|_| WebClassReauthSyncError::AuthenticationFailed)?;
    *client = new_client;
    run(client).map_err(WebClassReauthSyncError::Sync)
}
