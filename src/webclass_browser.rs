//! Isolated browser adapter for read-only WebClass checks.

use std::{
    collections::BTreeMap,
    env, io,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};

use crate::{
    notion::state::{StateError, TargetLock, check_path},
    sync::{
        AcquiredAssignments, AcquiredWebClassCourses, WebClassAcquisition,
        WebClassAcquisitionAttempt, WebClassCourseListError, WebClassSyncError,
        validate_webclass_contents_json, validate_webclass_courses_json,
    },
    university_network::{
        BodyKind, HOPPII_ORIGIN, IDP_ORIGIN, MAX_COURSES, MAX_RECORDS, WEBCLASS_ORIGIN,
    },
    webclass::WebClassCourse,
    webclass_auth::{USER_PAGE, WEBCLASS_SSO_POST_PATH},
    webclass_http::{CONTENTS_PATH, COURSES_PATH, DASHBOARD_PATH, contents_url},
};

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

const DRIVER: &str = concat!(include_str!("webclass_browser.mjs"), "\nawait runDriver();");
const PROFILE_OWNER_MARKER: &str = "assignment-manager-webclass-profile-v1";

#[derive(Clone, Copy)]
pub enum ProfileMode {
    Temporary,
    Retained,
    Authenticate,
    Headless,
}

impl ProfileMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Temporary => "temporary",
            Self::Retained | Self::Authenticate | Self::Headless => "retained",
        }
    }
}

#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum BrowserOperation {
    Browse,
    Authenticate,
    Headless,
}

enum BrowserProfile {
    Temporary(tempfile::TempDir),
    Retained { path: PathBuf, _lock: TargetLock },
}

impl BrowserProfile {
    fn open(mode: ProfileMode) -> Result<Self, &'static str> {
        match mode {
            ProfileMode::Temporary => {
                let base = std::env::temp_dir();
                #[cfg(target_os = "macos")]
                let base = base.canonicalize().map_err(|_| "browser_setup")?;
                if !base.is_absolute() {
                    return Err("profile_path_unavailable");
                }
                tempfile::Builder::new()
                    .prefix("assignment-manager-webclass-")
                    .tempdir_in(base)
                    .map(Self::Temporary)
                    .map_err(|_| "browser_setup")
            }
            ProfileMode::Retained | ProfileMode::Authenticate => {
                Self::retained(&platform::retained_base()?, true)
            }
            ProfileMode::Headless => Self::retained(&platform::retained_base()?, false),
        }
    }

    fn retained(local: &Path, create: bool) -> Result<Self, &'static str> {
        let path = local.join(platform::PROFILE_DIRECTORY);
        let marker = path.join(PROFILE_OWNER_MARKER);
        check_path(&marker).map_err(|_| "profile_path_unavailable")?;
        if !local.is_dir() {
            return Err("profile_path_unavailable");
        }
        // Claim only a new directory. Never initialize any pre-existing profile in place.
        if create {
            match platform::create_private_dir(&path) {
                Ok(()) => platform::create_private_dir(&marker).map_err(|_| "profile_setup")?,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err("profile_setup"),
            }
        } else if let Err(error) = path.symlink_metadata() {
            return Err(if error.kind() == io::ErrorKind::NotFound {
                "reauth_required"
            } else {
                "profile_path_unavailable"
            });
        }
        platform::verify_profile_owner(&path, &marker)?;
        let _lock = TargetLock::acquire(&path).map_err(|error| match error {
            StateError::Busy => "profile_in_use",
            StateError::InvalidPath => "profile_path_unavailable",
            _ => "profile_setup",
        })?;
        Ok(Self::Retained { path, _lock })
    }

    fn path(&self) -> &Path {
        match self {
            Self::Temporary(directory) => directory.path(),
            Self::Retained { path, .. } => path,
        }
    }

    fn close(self) -> io::Result<()> {
        match self {
            Self::Temporary(directory) => directory.close(),
            Self::Retained { .. } => Ok(()),
        }
    }
}

pub struct BrowserCheck<T> {
    pub courses_get: u8,
    pub contents_get: u8,
    pub lms_reached: bool,
    pub navigation: Option<BrowserNavigation>,
    pub result: Result<T, &'static str>,
}

/// Only fixed navigation categories can cross the driver's result pipe.
#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BrowserNavigation {
    stage: NavigationStage,
    kind: NavigationKind,
    cause: NetworkCause,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_method: Option<NavigationMethod>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proxy_connect_observed: Option<NetworkCause>,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NavigationStage {
    Startup,
    Hoppii,
    Idp,
    WebclassSso,
    Webclass,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NavigationKind {
    TopLevelRequest,
    TopLevelCommit,
    BrowserErrorPage,
    BrowserInternalPage,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NetworkCause {
    DnsFailure,
    TlsFailure,
    ConnectionRefused,
    ConnectionReset,
    ConnectionClosed,
    ProxyFailure,
    Timeout,
    NavigationBlocked,
    UnknownNetworkFailure,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum NavigationMethod {
    Get,
    Post,
}

pub type BrowserCourseCheck = BrowserCheck<AcquiredWebClassCourses>;

pub struct BrowserCourseAssignments {
    pub course: WebClassCourse,
    pub assignments: AcquiredAssignments,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserConfig<'a> {
    executable: &'a Path,
    profile: &'a Path,
    temporary_profile: bool,
    operation: BrowserOperation,
    entry: String,
    origins: [&'a str; 3],
    lms: &'a str,
    courses_path: &'a str,
    contents_path: &'a str,
    dashboard_path: &'a str,
    login_path: &'a str,
    sso_post_path: &'a str,
    select_contents: bool,
    max_bytes: usize,
    login_millis: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserReply {
    reason: String,
    courses_get: u8,
    contents_get: u8,
    lms_reached: bool,
    navigation: Option<BrowserNavigation>,
    body: Option<String>,
    bodies: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum DriverMessage {
    Courses { body: String },
    Finished { reply: BrowserReply },
}

struct DriverResult {
    reply: BrowserReply,
    selection: Option<Result<SelectedCourses, &'static str>>,
}

#[derive(Clone, Copy)]
enum RequestedCourses<'a> {
    Single(&'a str),
    Confirmed(&'a BTreeMap<String, String>),
}

struct SelectedCourses {
    list: AcquiredWebClassCourses,
    ids: Vec<String>,
}

fn driver_command(profile: &BrowserProfile) -> Command {
    let mut command = platform::node_command(profile);
    command
        .args(["--input-type=module", "--eval", DRIVER])
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_DEBUG")
        .env_remove("NODE_DEBUG_NATIVE")
        .env_remove("SSLKEYLOGFILE")
        .env_remove("CHROME_LOG_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    command
}

fn run_driver(
    config: &BrowserConfig<'_>,
    profile: &BrowserProfile,
    request: Option<RequestedCourses<'_>>,
) -> Result<DriverResult, &'static str> {
    #[cfg(windows)]
    let job = platform::BrowserJob::new().ok_or("browser_setup")?;
    let mut child = driver_command(profile)
        .spawn()
        .map_err(|_| "browser_runtime_unavailable")?;
    #[cfg(windows)]
    if !job.add(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("browser_setup");
    }
    let mut stdout = BufReader::new(child.stdout.take().expect("piped browser result"));
    let stdin = child.stdin.take().expect("piped browser driver");
    let result = exchange_driver(&mut stdout, stdin, config, request);
    drop(stdout);
    #[cfg(windows)]
    if result.is_err() {
        drop(job);
        let _ = child.wait();
        return result;
    }
    let status = child.wait().map_err(|_| "browser_closed")?;
    #[cfg(windows)]
    drop(job);
    let result = result?;
    if !status.success() {
        return Err("browser_protocol");
    }
    Ok(result)
}

fn exchange_driver(
    stdout: &mut impl BufRead,
    mut stdin: impl Write,
    config: &BrowserConfig<'_>,
    request: Option<RequestedCourses<'_>>,
) -> Result<DriverResult, &'static str> {
    fn send(writer: &mut impl Write, value: &impl Serialize) -> Result<(), &'static str> {
        serde_json::to_writer(&mut *writer, value).map_err(|_| "browser_protocol")?;
        writer.write_all(b"\n").map_err(|_| "browser_protocol")
    }
    fn receive(reader: &mut impl BufRead, bound: usize) -> Result<DriverMessage, &'static str> {
        let mut line = Vec::new();
        reader
            .take(bound as u64)
            .read_until(b'\n', &mut line)
            .map_err(|_| "browser_protocol")?;
        if line.last() != Some(&b'\n') {
            return Err("browser_protocol");
        }
        serde_json::from_slice(&line).map_err(|_| "browser_protocol")
    }

    send(&mut stdin, config)?;
    let bound = config.max_bytes * 6 + 65536;
    let mut message = receive(stdout, bound)?;
    let mut selection = None;
    if let (Some(request), DriverMessage::Courses { body }) = (request, &message) {
        let courses = select_courses(body, request);
        let target = |group_id: &str| {
            let origin = reqwest::Url::parse(config.lms).expect("fixed WebClass origin");
            let url = contents_url(&origin, group_id);
            format!("{}?{}", url.path(), url.query().expect("group_id query"))
        };
        match (courses.as_ref().ok(), request) {
            (Some(courses), RequestedCourses::Single(_)) => {
                send(&mut stdin, &target(&courses.ids[0]))?
            }
            (Some(courses), RequestedCourses::Confirmed(_)) => {
                send(
                    &mut stdin,
                    &courses.ids.iter().map(|id| target(id)).collect::<Vec<_>>(),
                )?;
            }
            (None, _) => send(&mut stdin, &Option::<String>::None)?,
        }
        selection = Some(courses);
        message = receive(stdout, bound)?;
    }
    match message {
        DriverMessage::Finished { reply } => Ok(DriverResult { reply, selection }),
        DriverMessage::Courses { .. } => Err("browser_protocol"),
    }
}

fn parse_courses(body: &str) -> Result<AcquiredWebClassCourses, &'static str> {
    if body.len() > BodyKind::WebClassJson.limit() {
        return Err("course_response_limit");
    }
    validate_webclass_courses_json(body, |_| Ok(())).map_err(|error| match error {
        WebClassCourseListError::Limit => "course_record_limit",
        WebClassCourseListError::Validation(_) => "invalid_course_id",
        _ => "invalid_course_json",
    })
}

fn select_courses(
    body: &str,
    request: RequestedCourses<'_>,
) -> Result<SelectedCourses, &'static str> {
    let list = parse_courses(body)?;
    let ids: Vec<_> = list
        .records()
        .iter()
        .filter(|course| match request {
            RequestedCourses::Single(id) => course.group_id == id,
            RequestedCourses::Confirmed(groups) => groups
                .get(&course.group_id)
                .is_some_and(|term| *term == format!("{}:{}", course.year, course.semester)),
        })
        .map(|course| course.group_id.clone())
        .collect();
    if matches!(request, RequestedCourses::Single(_)) && ids.is_empty() {
        return Err("course_id_not_found");
    }
    Ok(SelectedCourses { list, ids })
}

impl BrowserReply {
    fn check_complete(&self, expected_contents_get: u8) -> Result<(), &'static str> {
        match self.reason.as_str() {
            "ok" if self.lms_reached
                && self.courses_get == 1
                && self.contents_get == expected_contents_get =>
            {
                Ok(())
            }
            _ => Err(self.diagnostic()),
        }
    }

    fn diagnostic(&self) -> &'static str {
        match self.reason.as_str() {
            "acquisition_context_changed" => "acquisition_context_changed",
            "auth_expired" => "auth_expired",
            "reauth_required" => "reauth_required",
            "access_denied" => "access_denied",
            "server_unavailable" => "server_unavailable",
            "network_failed" => "network_failed",
            "unexpected_api_redirect" => "unexpected_api_redirect",
            "browser_closed" => "browser_closed",
            "browser_navigation_failed" => "browser_navigation_failed",
            "course_request_failed" => "course_request_failed",
            "course_response_limit" => "course_response_limit",
            "contents_request_failed" => "contents_request_failed",
            "contents_response_limit" => "contents_response_limit",
            "edge_start_failed" => "edge_start_failed",
            "chrome_start_failed" => "chrome_start_failed",
            "invalid_course_response" => "invalid_course_response",
            "invalid_contents_response" => "invalid_contents_response",
            "login_timeout" => "login_timeout",
            "webclass_ready_timeout" => "webclass_ready_timeout",
            "acquisition_timeout" => "acquisition_timeout",
            "navigation_limit" => "navigation_limit",
            "navigation_method_blocked" => "navigation_method_blocked",
            "profile_startup_tabs" => "profile_startup_tabs",
            "profile_cleanup_failed" => "profile_cleanup_failed",
            "proxy_failure" => "proxy_failure",
            "unknown_destination" => "unknown_destination",
            _ => "browser_protocol",
        }
    }

    fn checked_body(&self, expected_contents_get: u8) -> Result<&str, &'static str> {
        self.check_complete(expected_contents_get)?;
        self.body
            .as_deref()
            .filter(|_| self.bodies.is_none())
            .ok_or("browser_protocol")
    }
}

fn parse_assignments(driver: DriverResult) -> Result<BrowserCourseAssignments, &'static str> {
    let DriverResult { reply, selection } = driver;
    if reply.reason == "course_selection_stopped"
        && reply.courses_get == 1
        && reply.contents_get == 0
        && reply.lms_reached
        && let Some(Err(reason)) = selection
    {
        return Err(reason);
    }
    let body = reply.checked_body(1)?;
    let selected = selection.and_then(Result::ok).ok_or("browser_protocol")?;
    let [id] = selected.ids.as_slice() else {
        return Err("browser_protocol");
    };
    let course = selected
        .list
        .records()
        .iter()
        .find(|course| course.group_id == *id)
        .cloned()
        .ok_or("browser_protocol")?;
    let assignments = parse_contents(body, &course.group_id)?;
    Ok(BrowserCourseAssignments {
        course,
        assignments,
    })
}

fn parse_contents(body: &str, group_id: &str) -> Result<AcquiredAssignments, &'static str> {
    if body.len() > BodyKind::WebClassJson.limit() {
        return Err("contents_response_limit");
    }
    validate_webclass_contents_json(body, group_id, |count| {
        if count > MAX_RECORDS {
            Err(WebClassSyncError::Validation)
        } else {
            Ok(())
        }
    })
    .map_err(|error| match error {
        WebClassSyncError::Parse => "invalid_contents_json",
        WebClassSyncError::Map => "invalid_contents_deadline",
        _ => "invalid_contents_group_or_identity",
    })
}

fn parse_batches(driver: DriverResult) -> Result<WebClassAcquisition, &'static str> {
    let DriverResult { reply, selection } = driver;
    if reply.reason == "course_selection_stopped"
        && let Some(Err(reason)) = selection
    {
        return Err(reason);
    }
    if reply.reason != "ok" {
        reply.check_complete(0)?;
    }
    let selected = selection.and_then(Result::ok).ok_or("browser_protocol")?;
    reply.check_complete(u8::try_from(selected.ids.len()).map_err(|_| "browser_protocol")?)?;
    let bodies = reply
        .bodies
        .filter(|bodies| reply.body.is_none() && bodies.len() == selected.ids.len())
        .ok_or("browser_protocol")?;
    if bodies.iter().map(String::len).sum::<usize>() > BodyKind::WebClassJson.limit() {
        return Err("contents_response_limit");
    }
    let groups = bodies
        .iter()
        .zip(&selected.ids)
        .map(|(body, group)| parse_contents(body, group))
        .collect::<Result<_, _>>()?;
    Ok(WebClassAcquisition::new(
        vec![WebClassAcquisitionAttempt {
            course_list: Some(selected.list),
            groups,
            result: Ok(()),
        }],
        Ok(()),
    ))
}

fn browser_executable() -> Result<PathBuf, &'static str> {
    platform::find_browser().ok_or(platform::UNAVAILABLE)
}

/// Checks the executable paths without starting the browser or Node.js.
pub fn check_local_dependencies() -> Result<(), &'static str> {
    browser_executable()?;
    let path = env::var_os("PATH").ok_or("browser_runtime_unavailable")?;
    env::split_paths(&path)
        .any(|directory| platform::is_executable(&directory.join(platform::NODE_EXECUTABLE)))
        .then_some(())
        .ok_or("browser_runtime_unavailable")
}

fn launch(
    mode: ProfileMode,
    request: Option<RequestedCourses<'_>>,
) -> Result<DriverResult, &'static str> {
    let executable = browser_executable()?;
    let profile = BrowserProfile::open(mode)?;
    let (operation, entry) = match mode {
        ProfileMode::Headless => (
            BrowserOperation::Headless,
            format!("{WEBCLASS_ORIGIN}{DASHBOARD_PATH}"),
        ),
        ProfileMode::Authenticate => (
            BrowserOperation::Authenticate,
            format!("{HOPPII_ORIGIN}{USER_PAGE}"),
        ),
        _ => (
            BrowserOperation::Browse,
            format!("{HOPPII_ORIGIN}{USER_PAGE}"),
        ),
    };
    let config = BrowserConfig {
        executable: &executable,
        profile: profile.path(),
        temporary_profile: matches!(mode, ProfileMode::Temporary),
        operation,
        entry,
        origins: [HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN],
        lms: WEBCLASS_ORIGIN,
        courses_path: COURSES_PATH,
        contents_path: CONTENTS_PATH,
        dashboard_path: DASHBOARD_PATH,
        login_path: "/webclass/login.php",
        sso_post_path: WEBCLASS_SSO_POST_PATH,
        select_contents: request.is_some(),
        max_bytes: BodyKind::WebClassJson.limit(),
        login_millis: 5 * 60 * 1000,
    };
    let mut report = run_driver(&config, &profile, request);
    if profile.close().is_err() {
        match &mut report {
            Ok(report) => report.reply.reason = "profile_cleanup_failed".into(),
            Err(reason) => *reason = "profile_cleanup_failed",
        }
    }
    report
}

fn check<T>(
    mode: ProfileMode,
    request: Option<RequestedCourses<'_>>,
    parse: impl FnOnce(DriverResult) -> Result<T, &'static str>,
) -> BrowserCheck<T> {
    match launch(mode, request) {
        Ok(driver) => BrowserCheck {
            courses_get: driver.reply.courses_get,
            contents_get: driver.reply.contents_get,
            lms_reached: driver.reply.lms_reached,
            navigation: driver.reply.navigation,
            result: parse(driver),
        },
        Err(reason) => BrowserCheck {
            courses_get: 0,
            contents_get: 0,
            lms_reached: false,
            navigation: None,
            result: Err(reason),
        },
    }
}

/// Opens the official site in an app-owned browser profile and reads only the courses API.
pub fn run(mode: ProfileMode) -> BrowserCourseCheck {
    check(mode, None, |driver| {
        parse_courses(driver.reply.checked_body(0)?)
    })
}

/// Let the account holder establish a retained session without reading course data.
pub fn authenticate() -> BrowserCheck<()> {
    check(ProfileMode::Authenticate, None, |driver| {
        let reply = &driver.reply;
        if reply.reason == "authenticated"
            && reply.lms_reached
            && reply.courses_get == 0
            && reply.contents_get == 0
            && reply.body.is_none()
            && reply.bodies.is_none()
        {
            Ok(())
        } else {
            Err(reply.diagnostic())
        }
    })
}

/// Reads the assignments of one course identified by the source-native `group_id`.
pub fn run_for_course(
    mode: ProfileMode,
    course_id: &str,
) -> BrowserCheck<BrowserCourseAssignments> {
    check(
        mode,
        Some(RequestedCourses::Single(course_id)),
        parse_assignments,
    )
}

/// Read each confirmed group at most once, within one isolated Chrome session.
pub fn run_for_confirmed_courses(
    mode: ProfileMode,
    groups: &BTreeMap<String, String>,
) -> BrowserCheck<WebClassAcquisition> {
    debug_assert!(MAX_COURSES <= usize::from(u8::MAX));
    check(
        mode,
        Some(RequestedCourses::Confirmed(groups)),
        parse_batches,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    const COURSES: &str = include_str!("../fixtures/webclass/courses_basic.json");
    const CONTENTS: &str = include_str!("../fixtures/webclass/contents_basic.json");
    const SELECTED: &str = "fixture-course-002";

    #[test]
    fn retained_profile_survives_close_and_locks_exclusively() {
        let local = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let base = local.path().canonicalize().unwrap();
        #[cfg(windows)]
        let base = local.path().to_owned();
        assert_eq!(
            BrowserProfile::retained(&base, false).err(),
            Some("reauth_required")
        );
        assert!(!base.join(platform::PROFILE_DIRECTORY).exists());
        let first = BrowserProfile::retained(&base, true).unwrap();
        let path = first.path().to_owned();
        let browser_file = path.join("browser-owned-fixture");
        fs::write(&browser_file, "synthetic browser state").unwrap();
        assert_eq!(
            BrowserProfile::retained(&base, false).err(),
            Some("profile_in_use")
        );
        first.close().unwrap();
        let reopened = BrowserProfile::retained(&base, false).unwrap();
        assert_eq!(reopened.path(), path);
        assert_eq!(
            fs::read_to_string(browser_file).unwrap(),
            "synthetic browser state"
        );
    }

    #[test]
    fn retained_profile_rejects_unmanaged_path_and_temporary_profile_is_removed() {
        let local = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let base = local.path().canonicalize().unwrap();
        #[cfg(windows)]
        let base = local.path().to_owned();
        let foreign = base.join(platform::PROFILE_DIRECTORY);
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("test-fixture"), "unmanaged").unwrap();
        assert_eq!(
            BrowserProfile::retained(&base, true).err(),
            Some("profile_not_managed")
        );
        assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1);
        let temporary = BrowserProfile::open(ProfileMode::Temporary).unwrap();
        let path = temporary.path().to_owned();
        temporary.close().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn browser_reply_uses_shared_course_parser_and_validates_ids() {
        let reply = |body| BrowserReply {
            reason: "ok".into(),
            courses_get: 1,
            contents_get: 0,
            lms_reached: true,
            navigation: None,
            body: Some(body),
            bodies: None,
        };
        let courses = parse_courses(reply(COURSES.into()).checked_body(0).unwrap()).unwrap();
        assert_eq!(courses.records().len(), 3);
        let first = &courses.records()[0];
        assert_eq!(first.group_id, "fixture-course-001");
        assert_eq!(first.group_name, "線形代数");
        assert_eq!(first.year, 2026);
        assert_eq!(
            (
                first.semester.as_str(),
                first.day_of_week.as_str(),
                first.class_order.as_str()
            ),
            ("2", "1", "4")
        );
        assert_eq!(
            parse_courses(&COURSES.replace(SELECTED, "fixture-course-001")).err(),
            Some("invalid_course_id")
        );
        let mut noncanonical = reply(COURSES.into());
        noncanonical.courses_get = 2;
        assert_eq!(noncanonical.checked_body(0).err(), Some("browser_protocol"));
    }

    fn synthetic_exchange(
        courses: &str,
        course_id: &str,
        finish: serde_json::Value,
    ) -> (DriverResult, serde_json::Value) {
        synthetic_exchange_with(courses, RequestedCourses::Single(course_id), finish)
    }

    fn synthetic_exchange_with(
        courses: &str,
        requested: RequestedCourses<'_>,
        finish: serde_json::Value,
    ) -> (DriverResult, serde_json::Value) {
        let config = BrowserConfig {
            executable: Path::new("fixture-browser"),
            profile: Path::new("fixture-profile"),
            temporary_profile: true,
            operation: BrowserOperation::Browse,
            entry: "https://hoppii.synthetic.invalid/portal".into(),
            origins: [
                "https://hoppii.synthetic.invalid",
                "https://idp.synthetic.invalid",
                "https://webclass.synthetic.invalid",
            ],
            lms: "https://webclass.synthetic.invalid",
            courses_path: COURSES_PATH,
            contents_path: CONTENTS_PATH,
            dashboard_path: DASHBOARD_PATH,
            login_path: "/webclass/login.php",
            sso_post_path: WEBCLASS_SSO_POST_PATH,
            select_contents: true,
            max_bytes: BodyKind::WebClassJson.limit(),
            login_millis: 100,
        };
        let first = json!({"stage": "courses", "body": courses});
        let last = json!({"stage": "finished", "reply": finish});
        let replies = format!("{first}\n{last}\n");
        let mut commands = Vec::new();
        let driver = exchange_driver(
            &mut replies.as_bytes(),
            &mut commands,
            &config,
            Some(requested),
        )
        .unwrap();
        let commands = std::str::from_utf8(&commands)
            .unwrap()
            .lines()
            .collect::<Vec<_>>();
        assert_eq!(commands.len(), 2);
        (driver, serde_json::from_str(commands[1]).unwrap())
    }

    #[test]
    fn browser_batch_requires_exact_groups_and_terms_and_rejects_incomplete_results() {
        let groups = BTreeMap::from([
            ("fixture-course-001".into(), "2026:2".into()),
            (SELECTED.into(), "2026:2".into()),
            ("fixture-course-003".into(), "2026:1".into()),
            ("absent-course".into(), "2026:2".into()),
        ]);
        let request = RequestedCourses::Confirmed(&groups);
        let reply = |bodies| {
            json!({
                "reason": "ok", "courses_get": 1, "contents_get": 2, "lms_reached": true,
                "bodies": bodies,
            })
        };
        let (driver, targets) =
            synthetic_exchange_with(COURSES, request, reply(vec!["[]", CONTENTS]));
        assert_eq!(
            targets,
            json!([
                format!("{CONTENTS_PATH}?group_id=fixture-course-001"),
                format!("{CONTENTS_PATH}?group_id={SELECTED}"),
            ])
        );
        let acquired = parse_batches(driver).unwrap();
        let batches = acquired.attempts()[0].groups();
        assert!(batches[0].batch().as_slice().is_empty());
        assert_eq!(batches[1].batch().as_slice().len(), 3);
        for (bodies, diagnostic) in [
            (vec!["[]"], "browser_protocol"),
            (vec![CONTENTS, "[]"], "invalid_contents_group_or_identity"),
        ] {
            let (driver, _) = synthetic_exchange_with(COURSES, request, reply(bodies));
            assert_eq!(parse_batches(driver).err(), Some(diagnostic));
        }
    }

    #[test]
    fn browser_requests_only_the_validated_exact_group_and_maps_contents() {
        let group = " course &group_id=other+日本/%?# ";
        let courses = COURSES.replace(SELECTED, group);
        let contents = CONTENTS.replace(SELECTED, group);
        let (driver, target) = synthetic_exchange(
            &courses,
            group,
            json!({
                "reason": "ok", "courses_get": 1, "contents_get": 1, "lms_reached": true, "body": contents,
            }),
        );
        assert_eq!(
            target,
            format!(
                "{CONTENTS_PATH}?group_id=+course+%26group_id%3Dother%2B%E6%97%A5%E6%9C%AC%2F%25%3F%23+"
            )
        );
        let result = parse_assignments(driver).unwrap();
        assert_eq!(result.course.group_id, group);
        let batch = result.assignments.batch().as_slice();
        assert_eq!(batch.len(), 3);
        assert_eq!(batch[0].external_key.scope, group);
        assert_eq!(batch[0].title, "演習課題A");
        assert!(batch[0].deadline.is_some());
        assert!(
            batch
                .iter()
                .all(|assignment| assignment.submission_url.is_none())
        );
    }

    #[test]
    fn browser_navigation_diagnostics_accept_only_fixed_categories() {
        let fixture = json!({
            "reason": "browser_navigation_failed", "courses_get": 0, "contents_get": 0,
            "lms_reached": false, "navigation": {
                "stage": "webclass_sso", "kind": "browser_error_page", "cause": "proxy_failure",
                "request_method": "post", "proxy_connect_observed": "dns_failure",
            },
        });
        let reply: BrowserReply = serde_json::from_value(fixture.clone()).unwrap();
        assert_eq!(reply.checked_body(1), Err("browser_navigation_failed"));
        assert_eq!(
            serde_json::to_value(reply.navigation).unwrap(),
            fixture["navigation"]
        );

        for field in [
            "stage",
            "kind",
            "cause",
            "request_method",
            "proxy_connect_observed",
            "url",
        ] {
            let mut untrusted = fixture.clone();
            untrusted["navigation"][field] =
                json!("https://example.invalid/?token=fixture-private");
            assert!(serde_json::from_value::<BrowserReply>(untrusted).is_err());
        }
    }

    #[test]
    fn browser_sends_no_contents_target_for_missing_or_invalid_courses() {
        for (courses, id, diagnostic) in [
            (COURSES, "Fixture-course-002", "course_id_not_found"),
            (COURSES, "fixture-course-002 ", "course_id_not_found"),
            ("[]", SELECTED, "course_id_not_found"),
            ("[", SELECTED, "invalid_course_json"),
            (
                &COURSES.replace("fixture-course-003", SELECTED),
                SELECTED,
                "invalid_course_id",
            ),
        ] {
            let (driver, target) = synthetic_exchange(
                courses,
                id,
                json!({
                    "reason": "course_selection_stopped", "courses_get": 1, "contents_get": 0, "lms_reached": true,
                }),
            );
            assert!(target.is_null());
            assert_eq!(parse_assignments(driver).err(), Some(diagnostic));
        }
    }

    #[test]
    fn browser_validates_all_contents_before_display_and_accepts_empty_results() {
        let mut hidden_other: serde_json::Value = serde_json::from_str(CONTENTS).unwrap();
        hidden_other[2]["hidden_content"] = json!(true);
        hidden_other[2]["group_id"] = json!("fixture-course-003");
        for (body, diagnostic) in [
            (
                hidden_other.to_string(),
                "invalid_contents_group_or_identity",
            ),
            (
                CONTENTS.replace("2026-10-31", "2026-13-31"),
                "invalid_contents_deadline",
            ),
            ("[".into(), "invalid_contents_json"),
        ] {
            let (driver, _) = synthetic_exchange(
                COURSES,
                SELECTED,
                json!({
                    "reason": "ok", "courses_get": 1, "contents_get": 1, "lms_reached": true, "body": body,
                }),
            );
            assert_eq!(parse_assignments(driver).err(), Some(diagnostic));
        }
        let (empty, _) = synthetic_exchange(
            COURSES,
            SELECTED,
            json!({
                "reason": "ok", "courses_get": 1, "contents_get": 1, "lms_reached": true, "body": "[]",
            }),
        );
        assert!(
            parse_assignments(empty)
                .unwrap()
                .assignments
                .batch()
                .as_slice()
                .is_empty()
        );
    }
}
