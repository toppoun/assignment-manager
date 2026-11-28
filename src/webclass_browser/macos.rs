use std::{
    env,
    fs::{self, DirBuilder},
    io,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{DirBuilderExt, MetadataExt},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Command,
};

use super::{BrowserProfile, check_path};

pub(super) const PROFILE_DIRECTORY: &str = "AssignmentManagerWebClassChrome";
pub(super) const NODE_EXECUTABLE: &str = "node";
const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";

pub(super) fn find_browser() -> Option<PathBuf> {
    let path = PathBuf::from(CHROME);
    is_executable(&path).then_some(path)
}

pub(super) fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.mode() & 0o111 != 0)
}

pub(super) const UNAVAILABLE: &str = "chrome_unavailable";

pub(super) fn retained_base() -> Result<PathBuf, &'static str> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("profile_path_unavailable")?;
    let base = home.join("Library/Application Support");
    check_path(&base).map_err(|_| "profile_path_unavailable")?;
    if !home.is_dir() {
        return Err("profile_path_unavailable");
    }
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&base)
        .map_err(|_| "profile_path_unavailable")?;
    Ok(base)
}

pub(super) fn create_private_dir(path: &Path) -> io::Result<()> {
    DirBuilder::new().mode(0o700).create(path)
}

pub(super) fn verify_profile_owner(path: &Path, marker: &Path) -> Result<(), &'static str> {
    let uid = unsafe { libc::geteuid() };
    for directory in [path, marker] {
        let meta = fs::symlink_metadata(directory).map_err(|_| "profile_not_managed")?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err("profile_not_managed");
        }
    }
    Ok(())
}

/// Compare local filesystem identities without printing the user or any path.
pub(super) fn profile_tag(path: &Path) -> Option<String> {
    use aws_lc_rs::digest::{Context, SHA256};
    use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};

    let mut hash = Context::new(&SHA256);
    hash.update(b"assignment-manager-webclass-profile-v1\0");
    for directory in [path.ancestors().nth(3)?, path] {
        let metadata = fs::metadata(directory).ok()?;
        hash.update(&metadata.uid().to_be_bytes());
        hash.update(&metadata.dev().to_be_bytes());
        hash.update(&metadata.ino().to_be_bytes());
    }
    Some(format!(
        "v1-{}",
        URL_SAFE_NO_PAD.encode(&hash.finish().as_ref()[..16])
    ))
}

pub(super) fn node_command(profile: &BrowserProfile) -> Command {
    let mut node = Command::new(NODE_EXECUTABLE);
    // The driver handles CLI termination via stdin EOF, including terminal Ctrl+C.
    node.process_group(0);
    if let BrowserProfile::Retained { _lock: lock, .. } = profile {
        let fd = lock.as_fd().as_raw_fd();
        // Keep the same flock in the driver until Chrome shuts down, even if Rust exits first.
        // fcntl changes close-on-exec only in the child and is safe to call after fork.
        unsafe {
            node.pre_exec(move || {
                if libc::fcntl(fd, libc::F_SETFD, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    node
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        net::{Ipv4Addr, TcpListener},
        os::unix::fs::{PermissionsExt, symlink},
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::webclass_browser::{
        BrowserConfig, BrowserOperation, DriverMessage, ProfileMode, driver_command, run_driver,
    };

    #[test]
    fn retained_profile_tag_identifies_same_home_and_profile_after_reopening() {
        let local = tempfile::tempdir().unwrap();
        let base = local
            .path()
            .canonicalize()
            .unwrap()
            .join("Library/Application Support");
        fs::create_dir_all(&base).unwrap();
        let first = BrowserProfile::retained(&base, true).unwrap();
        let tag = profile_tag(first.path()).unwrap();
        first.close().unwrap();
        let reopened = BrowserProfile::retained(&base, false).unwrap();
        assert_eq!(profile_tag(reopened.path()).as_deref(), Some(tag.as_str()));
        assert!(tag.starts_with("v1-") && tag.len() == 25);
        let other_base = base.join("other");
        fs::create_dir(&other_base).unwrap();
        let other = BrowserProfile::retained(&other_base, true).unwrap();
        assert_ne!(profile_tag(other.path()), Some(tag));
    }

    #[test]
    fn retained_profile_rejects_a_public_directory_or_redirected_marker() {
        let local = tempfile::tempdir().unwrap();
        let base = local.path().canonicalize().unwrap();
        let profile = BrowserProfile::retained(&base, true).unwrap();
        let path = profile.path().to_owned();
        profile.close().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            BrowserProfile::retained(&base, false).err(),
            Some("profile_not_managed")
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let marker = path.join(super::super::PROFILE_OWNER_MARKER);
        fs::remove_dir(&marker).unwrap();
        symlink(&base, marker).unwrap();
        assert_eq!(
            BrowserProfile::retained(&base, false).err(),
            Some("profile_path_unavailable")
        );
    }

    #[test]
    #[ignore = "requires an interactive macOS session, Chrome, Node.js and loopback sockets"]
    fn chrome_driver_cleans_up_when_the_cli_disconnects() {
        let chrome = find_browser().expect("installed Chrome");
        let local = tempfile::tempdir().unwrap();
        let base = local.path().canonicalize().unwrap();
        for temporary_profile in [true, false] {
            let profile = if temporary_profile {
                BrowserProfile::open(ProfileMode::Temporary).unwrap()
            } else {
                BrowserProfile::retained(&base, true).unwrap()
            };
            let path = profile.path().to_owned();
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let origin = format!("https://{}", listener.local_addr().unwrap());
            let mut config = BrowserConfig {
                executable: &chrome,
                profile: &path,
                temporary_profile,
                operation: BrowserOperation::Browse,
                entry: format!("{origin}/portal"),
                origins: [&origin; 3],
                lms: &origin,
                courses_path: "/webclass/courses",
                contents_path: "/webclass/contents",
                dashboard_path: "/webclass/dashboard",
                login_path: "/webclass/login.php",
                sso_post_path: "/webclass/singlesignon.php",
                select_contents: false,
                max_bytes: 4096,
                login_millis: 12000,
            };
            let mut driver = driver_command(&profile).spawn().unwrap();
            let mut input = driver.stdin.take().unwrap();
            serde_json::to_writer(&mut input, &config).unwrap();
            input.write_all(b"\n").unwrap();
            let deadline = Instant::now() + Duration::from_secs(8);
            let _socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "Chrome did not connect");
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(error) => panic!("local fixture listener: {error}"),
                }
            };
            if let BrowserProfile::Temporary(directory) = profile {
                assert_eq!(directory.keep(), path);
            } else {
                drop(profile);
                assert_eq!(
                    BrowserProfile::retained(&base, false).err(),
                    Some("profile_in_use")
                );
            }
            drop(input);
            let output = driver.wait_with_output().unwrap();
            assert!(output.status.success());
            let DriverMessage::Finished { reply } = serde_json::from_slice(&output.stdout).unwrap()
            else {
                panic!("driver did not report completion");
            };
            assert_eq!(reply.reason, "browser_closed");
            assert_eq!((reply.courses_get, reply.contents_get), (0, 0));
            if temporary_profile {
                assert!(!path.exists());
            } else {
                let profile = BrowserProfile::retained(&base, false).unwrap();
                assert_eq!(profile.path(), path);
                let missing = base.join("missing-chrome");
                config.executable = &missing;
                let result = run_driver(&config, &profile, None).unwrap();
                assert_eq!(result.reply.checked_body(0), Err("chrome_start_failed"));
            }
        }
    }
}
