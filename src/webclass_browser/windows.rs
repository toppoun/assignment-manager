use std::{
    env, fs,
    os::windows::{
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, Command},
    ptr,
};

use windows_sys::Win32::System::{
    JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    },
    Threading::CREATE_NO_WINDOW,
};

use super::BrowserProfile;

pub(super) const PROFILE_DIRECTORY: &str = "AssignmentManagerWebClassEdge";
pub(super) const UNAVAILABLE: &str = "edge_unavailable";
pub(super) const NODE_EXECUTABLE: &str = "node.exe";

pub(super) fn find_browser() -> Option<PathBuf> {
    ["ProgramFiles(x86)", "ProgramW6432", "ProgramFiles"]
        .iter()
        .filter_map(env::var_os)
        .map(|base| PathBuf::from(base).join("Microsoft/Edge/Application/msedge.exe"))
        .find(|path| path.is_absolute() && is_executable(path))
}

pub(super) fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub(super) fn retained_base() -> Result<PathBuf, &'static str> {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .ok_or("profile_path_unavailable")
}

pub(super) use fs::create_dir as create_private_dir;

pub(super) fn verify_profile_owner(_path: &Path, marker: &Path) -> Result<(), &'static str> {
    marker.is_dir().then_some(()).ok_or("profile_not_managed")
}

pub(super) fn node_command(_profile: &BrowserProfile) -> Command {
    let mut node = Command::new(NODE_EXECUTABLE);
    node.creation_flags(CREATE_NO_WINDOW);
    node
}

pub(super) struct BrowserJob(OwnedHandle);

impl BrowserJob {
    pub(super) fn new() -> Option<Self> {
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

    pub(super) fn add(&self, child: &Child) -> bool {
        unsafe { AssignProcessToJobObject(self.0.as_raw_handle(), child.as_raw_handle()) != 0 }
    }
}
