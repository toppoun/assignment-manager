//! Windows Edge adapter for the read-only course check.

use std::{
    env, fs, io,
    io::{Read, Write},
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
    sync::{AcquiredWebClassCourses, validate_webclass_courses_json},
    university_network::{BodyKind, HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN},
    webclass_auth::USER_PAGE,
    webclass_http::{COURSES_PATH, DASHBOARD_PATH},
};

const DRIVER: &str = include_str!("webclass_browser.mjs");
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

pub struct BrowserCourseCheck {
    pub courses_get: u8,
    pub lms_reached: bool,
    pub result: Result<AcquiredWebClassCourses, &'static str>,
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
    dashboard_path: &'a str,
    max_bytes: usize,
    login_millis: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserReply {
    reason: String,
    courses_get: u8,
    lms_reached: bool,
    body: Option<String>,
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

fn run_driver(config: &BrowserConfig<'_>) -> Result<BrowserReply, &'static str> {
    let input = serde_json::to_string(config).map_err(|_| "browser_setup")?;
    let job = BrowserJob::new().ok_or("browser_setup")?;
    let mut child = Command::new("node.exe")
        .args(["--input-type=module", "-"])
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
    let mut stdout = child.stdout.take().expect("piped browser result");
    let mut stdin = child.stdin.take().expect("piped browser driver");
    // Node reads the entire script from stdin before it can start Edge.
    let written = writeln!(
        stdin,
        "{DRIVER}\nprocess.stdout.write(JSON.stringify(await runBrowser({input})));"
    );
    drop(stdin);
    let bound = config.max_bytes * 6 + 65536;
    let mut result = Vec::new();
    let read = written.and_then(|()| {
        Read::by_ref(&mut stdout)
            .take((bound + 1) as u64)
            .read_to_end(&mut result)
    });
    drop(stdout);
    if read.is_err() || result.len() > bound {
        drop(job);
        let _ = child.wait();
        return Err("browser_protocol");
    }
    let status = child.wait().map_err(|_| "browser_closed")?;
    drop(job);
    if !status.success() {
        return Err("browser_protocol");
    }
    serde_json::from_slice(&result).map_err(|_| "browser_protocol")
}

fn parse_reply(reply: BrowserReply) -> BrowserCourseCheck {
    let result = match (reply.reason.as_str(), reply.body) {
        ("ok", Some(body)) if reply.courses_get == 1 && reply.lms_reached => {
            if body.len() > BodyKind::WebClassJson.limit() {
                Err("course_response_limit")
            } else {
                validate_webclass_courses_json(&body, |_| Ok(())).map_err(|error| match error {
                    crate::sync::WebClassCourseListError::Limit => "course_record_limit",
                    crate::sync::WebClassCourseListError::Validation(_) => "invalid_course_id",
                    _ => "invalid_course_json",
                })
            }
        }
        ("auth_expired", _) => Err("auth_expired"),
        ("browser_closed", _) => Err("browser_closed"),
        ("course_request_failed", _) => Err("course_request_failed"),
        ("course_response_limit", _) => Err("course_response_limit"),
        ("edge_start_failed", _) => Err("edge_start_failed"),
        ("invalid_course_response", _) => Err("invalid_course_response"),
        ("login_timeout", _) => Err("login_timeout"),
        ("navigation_limit", _) => Err("navigation_limit"),
        ("profile_startup_tabs", _) => Err("profile_startup_tabs"),
        ("unknown_destination", _) => Err("unknown_destination"),
        _ => Err("browser_protocol"),
    };
    BrowserCourseCheck {
        courses_get: reply.courses_get,
        lms_reached: reply.lms_reached,
        result,
    }
}

fn launch(mode: ProfileMode) -> Result<BrowserCourseCheck, &'static str> {
    let edge = find_edge().ok_or("edge_unavailable")?;
    let profile = EdgeProfile::open(mode)?;
    let config = BrowserConfig {
        edge: &edge,
        profile: profile.path(),
        entry: format!("{HOPPII_ORIGIN}{USER_PAGE}"),
        origins: [HOPPII_ORIGIN, IDP_ORIGIN, WEBCLASS_ORIGIN],
        lms: WEBCLASS_ORIGIN,
        courses_path: COURSES_PATH,
        dashboard_path: DASHBOARD_PATH,
        max_bytes: BodyKind::WebClassJson.limit(),
        login_millis: 5 * 60 * 1000,
    };
    let mut report = run_driver(&config).map(parse_reply);
    if profile.close().is_err() {
        match &mut report {
            Ok(report) => report.result = Err("profile_cleanup_failed"),
            Err(reason) => *reason = "profile_cleanup_failed",
        }
    }
    report
}

/// Opens the official site in an app-owned Edge profile and reads only the courses API.
pub fn run(mode: ProfileMode) -> BrowserCourseCheck {
    launch(mode).unwrap_or_else(|reason| BrowserCourseCheck {
        courses_get: 0,
        lms_reached: false,
        result: Err(reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let fixture = include_str!("../fixtures/webclass/courses_basic.json");
        let reply = |body| BrowserReply {
            reason: "ok".into(),
            courses_get: 1,
            lms_reached: true,
            body: Some(body),
        };
        let courses = parse_reply(reply(fixture.into())).result.unwrap();
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
            parse_reply(reply(
                fixture.replace("fixture-course-002", "fixture-course-001")
            ))
            .result
            .err(),
            Some("invalid_course_id")
        );
        let mut noncanonical = reply(fixture.into());
        noncanonical.courses_get = 2;
        assert_eq!(
            parse_reply(noncanonical).result.err(),
            Some("browser_protocol")
        );
    }
}
