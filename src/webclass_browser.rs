//! Windows Edge adapter for read-only WebClass checks.

use std::{
    env, fs, io,
    io::{BufRead, BufReader, Read, Write},
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr,
};

use serde::{Deserialize, Serialize};
use windows_sys::Win32::System::{
    JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    },
    Threading::CREATE_NO_WINDOW,
};

use crate::{
    notion::state::{StateError, TargetLock, check_path},
    sync::{
        AcquiredAssignments, AcquiredWebClassCourses, WebClassCourseListError, WebClassSyncError,
        validate_webclass_contents_json, validate_webclass_courses_json,
    },
    university_network::{BodyKind, HOPPII_ORIGIN, IDP_ORIGIN, MAX_RECORDS, WEBCLASS_ORIGIN},
    webclass::WebClassCourse,
    webclass_auth::{USER_PAGE, WEBCLASS_SSO_POST_PATH},
    webclass_http::{CONTENTS_PATH, COURSES_PATH, DASHBOARD_PATH, contents_url},
};

const DRIVER: &str = concat!(include_str!("webclass_browser.mjs"), "\nawait runDriver();");
const PROFILE_DIRECTORY: &str = "AssignmentManagerWebClassEdge";
const PROFILE_OWNER_MARKER: &str = "assignment-manager-webclass-profile-v1";

#[derive(Clone, Copy)]
pub enum ProfileMode {
    Temporary,
    Retained,
}

enum EdgeProfile {
    Temporary(tempfile::TempDir),
    Retained { path: PathBuf, _lock: TargetLock },
}

impl EdgeProfile {
    fn open(mode: ProfileMode) -> Result<Self, &'static str> {
        match mode {
            ProfileMode::Temporary => tempfile::Builder::new()
                .prefix("assignment-manager-webclass-")
                .tempdir()
                .map(Self::Temporary)
                .map_err(|_| "browser_setup"),
            ProfileMode::Retained => {
                let local = env::var_os("LOCALAPPDATA")
                    .map(PathBuf::from)
                    .ok_or("profile_path_unavailable")?;
                Self::retained(&local)
            }
        }
    }

    fn retained(local: &Path) -> Result<Self, &'static str> {
        let path = local.join(PROFILE_DIRECTORY);
        let marker = path.join(PROFILE_OWNER_MARKER);
        check_path(&marker).map_err(|_| "profile_path_unavailable")?;
        if !local.is_dir() {
            return Err("profile_path_unavailable");
        }
        // Claim only a new directory. Never initialize any pre-existing profile in place.
        match fs::create_dir(&path) {
            Ok(()) => fs::create_dir(&marker).map_err(|_| "profile_setup")?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err("profile_setup"),
        }
        if !marker.is_dir() {
            return Err("profile_not_managed");
        }
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
    edge: &'a Path,
    profile: &'a Path,
    entry: String,
    origins: [&'a str; 3],
    lms: &'a str,
    courses_path: &'a str,
    contents_path: &'a str,
    dashboard_path: &'a str,
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
}

#[derive(Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum DriverMessage {
    Courses { body: String },
    Finished { reply: BrowserReply },
}

struct DriverResult {
    reply: BrowserReply,
    selection: Option<Result<WebClassCourse, &'static str>>,
}

struct BrowserJob(OwnedHandle);

impl BrowserJob {
    fn new() -> Option<Self> {
        // The kill-on-close job also closes the isolated browser if the CLI is interrupted.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                ptr::from_ref(&limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        (configured != 0).then_some(Self(handle))
    }

    fn add(&self, child: &std::process::Child) -> bool {
        unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) != 0 }
    }
}

fn find_edge() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"]
        .iter()
        .filter_map(env::var_os)
        .map(|base| PathBuf::from(base).join("Microsoft/Edge/Application/msedge.exe"))
        .find(|path| path.is_absolute() && path.is_file())
}

fn run_driver(
    config: &BrowserConfig<'_>,
    course_id: Option<&str>,
) -> Result<DriverResult, &'static str> {
    let job = BrowserJob::new().ok_or("browser_setup")?;
    let mut child = Command::new("node.exe")
        .args(["--input-type=module", "--eval", DRIVER])
        .creation_flags(CREATE_NO_WINDOW)
        .env_remove("NODE_OPTIONS")
        .env_remove("NODE_DEBUG")
        .env_remove("NODE_DEBUG_NATIVE")
        .env_remove("SSLKEYLOGFILE")
        .env_remove("CHROME_LOG_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "browser_runtime_unavailable")?;
    if !job.add(&child) {
        let _ = child.kill();
        let _ = child.wait();
        return Err("browser_setup");
    }
    let mut stdout = BufReader::new(child.stdout.take().expect("piped browser result"));
    let stdin = child.stdin.take().expect("piped browser driver");
    let result = exchange_driver(&mut stdout, stdin, config, course_id);
    drop(stdout);
    if result.is_err() {
        drop(job);
        let _ = child.wait();
        return result;
    }
    let status = child.wait().map_err(|_| "browser_closed")?;
    drop(job);
    if !status.success() {
        return Err("browser_protocol");
    }
    result
}

fn exchange_driver(
    stdout: &mut impl BufRead,
    mut stdin: impl Write,
    config: &BrowserConfig<'_>,
    course_id: Option<&str>,
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
    if let (Some(course_id), DriverMessage::Courses { body }) = (course_id, &message) {
        let course = select_course(body, course_id);
        let target = course.as_ref().ok().map(|course| {
            let origin = reqwest::Url::parse(config.lms).expect("fixed WebClass origin");
            let url = contents_url(&origin, &course.group_id);
            format!("{}?{}", url.path(), url.query().expect("group_id query"))
        });
        send(&mut stdin, &target)?;
        selection = Some(course);
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

fn select_course(body: &str, course_id: &str) -> Result<WebClassCourse, &'static str> {
    parse_courses(body)?
        .records()
        .iter()
        .find(|course| course.group_id == course_id)
        .cloned()
        .ok_or("course_id_not_found")
}

impl BrowserReply {
    fn checked_body(&self, expected_contents_get: u8) -> Result<&str, &'static str> {
        match self.reason.as_str() {
            "ok" if self.lms_reached
                && self.courses_get == 1
                && self.contents_get == expected_contents_get =>
            {
                self.body.as_deref().ok_or("browser_protocol")
            }
            "acquisition_context_changed" => Err("acquisition_context_changed"),
            "auth_expired" => Err("auth_expired"),
            "browser_closed" => Err("browser_closed"),
            "browser_navigation_failed" => Err("browser_navigation_failed"),
            "course_request_failed" => Err("course_request_failed"),
            "course_response_limit" => Err("course_response_limit"),
            "contents_request_failed" => Err("contents_request_failed"),
            "contents_response_limit" => Err("contents_response_limit"),
            "edge_start_failed" => Err("edge_start_failed"),
            "invalid_course_response" => Err("invalid_course_response"),
            "invalid_contents_response" => Err("invalid_contents_response"),
            "login_timeout" => Err("login_timeout"),
            "navigation_limit" => Err("navigation_limit"),
            "navigation_method_blocked" => Err("navigation_method_blocked"),
            "profile_startup_tabs" => Err("profile_startup_tabs"),
            "profile_cleanup_failed" => Err("profile_cleanup_failed"),
            "proxy_failure" => Err("proxy_failure"),
            "unknown_destination" => Err("unknown_destination"),
            _ => Err("browser_protocol"),
        }
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
    let course = selection.and_then(Result::ok).ok_or("browser_protocol")?;
    if body.len() > BodyKind::WebClassJson.limit() {
        return Err("contents_response_limit");
    }
    let assignments = validate_webclass_contents_json(body, &course.group_id, |count| {
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
    })?;
    Ok(BrowserCourseAssignments {
        course,
        assignments,
    })
}

fn launch(mode: ProfileMode, course_id: Option<&str>) -> Result<DriverResult, &'static str> {
    let edge = find_edge().ok_or("edge_unavailable")?;
    let profile = EdgeProfile::open(mode)?;
    let config = BrowserConfig {
        edge: &edge,
        profile: profile.path(),
        entry: format!("{HOPPII_ORIGIN}{USER_PAGE}"),
        origins: [HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN],
        lms: WEBCLASS_ORIGIN,
        courses_path: COURSES_PATH,
        contents_path: CONTENTS_PATH,
        dashboard_path: DASHBOARD_PATH,
        sso_post_path: WEBCLASS_SSO_POST_PATH,
        select_contents: course_id.is_some(),
        max_bytes: BodyKind::WebClassJson.limit(),
        login_millis: 5 * 60 * 1000,
    };
    let mut report = run_driver(&config, course_id);
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
    course_id: Option<&str>,
    parse: impl FnOnce(DriverResult) -> Result<T, &'static str>,
) -> BrowserCheck<T> {
    match launch(mode, course_id) {
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

/// Opens the official site in an app-owned Edge profile and reads only the courses API.
pub fn run(mode: ProfileMode) -> BrowserCourseCheck {
    check(mode, None, |driver| {
        parse_courses(driver.reply.checked_body(0)?)
    })
}

/// Reads the assignments of one course identified by the source-native `group_id`.
pub fn run_for_course(
    mode: ProfileMode,
    course_id: &str,
) -> BrowserCheck<BrowserCourseAssignments> {
    check(mode, Some(course_id), parse_assignments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const COURSES: &str = include_str!("../fixtures/webclass/courses_basic.json");
    const CONTENTS: &str = include_str!("../fixtures/webclass/contents_basic.json");
    const SELECTED: &str = "fixture-course-002";

    #[test]
    fn retained_profile_survives_close_and_locks_exclusively() {
        let local = tempfile::tempdir().unwrap();
        let first = EdgeProfile::retained(local.path()).unwrap();
        let path = first.path().to_owned();
        let browser_file = path.join("edge-owned-fixture");
        fs::write(&browser_file, "synthetic Edge-owned state").unwrap();
        assert_eq!(
            EdgeProfile::retained(local.path()).err(),
            Some("profile_in_use")
        );
        first.close().unwrap();
        let reopened = EdgeProfile::retained(local.path()).unwrap();
        assert_eq!(reopened.path(), path);
        assert_eq!(
            fs::read_to_string(browser_file).unwrap(),
            "synthetic Edge-owned state"
        );
    }

    #[test]
    fn retained_profile_rejects_unmanaged_path_and_temporary_profile_is_removed() {
        let local = tempfile::tempdir().unwrap();
        let foreign = local.path().join(PROFILE_DIRECTORY);
        fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("test-fixture"), "unmanaged").unwrap();
        assert_eq!(
            EdgeProfile::retained(local.path()).err(),
            Some("profile_not_managed")
        );
        assert_eq!(fs::read_dir(&foreign).unwrap().count(), 1);
        let temporary = EdgeProfile::open(ProfileMode::Temporary).unwrap();
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
        let config = BrowserConfig {
            edge: Path::new("fixture-edge.exe"),
            profile: Path::new("fixture-profile"),
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
            Some(course_id),
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
