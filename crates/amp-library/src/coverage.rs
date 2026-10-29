//! Positive enumeration evidence for reconciling filesystem refreshes.
use crate::ScanProgress;
use amp_core::ScanIssue;
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, ReadDir},
    io,
    path::{Path, PathBuf},
};

#[derive(Debug, Default)]
pub struct DirectoryCoverage {
    pub complete: bool,
    pub children: BTreeSet<PathBuf>,
}

#[derive(Debug)]
pub struct Coverage {
    pub root: PathBuf,
    pub directories: BTreeMap<PathBuf, DirectoryCoverage>,
    pub files: BTreeSet<PathBuf>,
    pub failures: BTreeMap<PathBuf, String>,
    pub issues: Vec<ScanIssue>,
    pub source_disconnected: bool,
    pub source_identity: Option<String>,
    /// Root and nested mount boundaries observed during this refresh.
    pub mounts: BTreeMap<PathBuf, String>,
}

pub fn enumerate(
    root: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(ScanProgress),
) -> Result<Coverage> {
    enumerate_with(root, cancelled, progress, &mut |path| fs::read_dir(path))
}

fn enumerate_with(
    root: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(ScanProgress),
    read_directory: &mut dyn FnMut(&Path) -> io::Result<ReadDir>,
) -> Result<Coverage> {
    enumerate_with_identity(
        root,
        cancelled,
        progress,
        read_directory,
        &mut source_identity,
    )
}

fn enumerate_with_identity(
    root: &Path,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(ScanProgress),
    read_directory: &mut dyn FnMut(&Path) -> io::Result<ReadDir>,
    identity: &mut dyn FnMut(&Path) -> io::Result<String>,
) -> Result<Coverage> {
    let initial_identity = identity(root).context("Cannot identify the music source")?;
    let mut coverage = Coverage {
        root: root.into(),
        directories: BTreeMap::new(),
        files: BTreeSet::new(),
        failures: BTreeMap::new(),
        issues: vec![],
        source_disconnected: false,
        source_identity: Some(initial_identity.clone()),
        mounts: BTreeMap::from([(root.to_owned(), initial_identity)]),
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        if cancelled() {
            bail!("Scan cancelled");
        }
        let mut evidence = DirectoryCoverage {
            complete: true,
            children: BTreeSet::new(),
        };
        match identity(&directory) {
            Ok(current) => {
                let previous = coverage
                    .mounts
                    .iter()
                    .filter(|(path, _)| directory.starts_with(path))
                    .max_by_key(|(path, _)| path.components().count())
                    .map(|(_, identity)| identity);
                if previous != Some(&current) {
                    if directory == root {
                        coverage.source_disconnected = true;
                        coverage
                            .issue(&directory, "The music source changed during refresh".into());
                        evidence.complete = false;
                    } else {
                        coverage.mounts.insert(directory.clone(), current);
                    }
                }
            }
            Err(error) => {
                evidence.complete = false;
                coverage.directory_error(&directory, &error);
                coverage.directories.insert(directory, evidence);
                if coverage.source_disconnected {
                    break;
                }
                continue;
            }
        }
        if coverage.source_disconnected {
            coverage.directories.insert(directory, evidence);
            break;
        }
        match read_directory(&directory) {
            Err(error) => {
                evidence.complete = false;
                coverage.directory_error(&directory, &error);
            }
            Ok(entries) => {
                for entry in entries {
                    if cancelled() {
                        bail!("Scan cancelled");
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            evidence.complete = false;
                            coverage.directory_error(&directory, &error);
                            if coverage.source_disconnected {
                                break;
                            }
                            continue;
                        }
                    };
                    let path = entry.path();
                    evidence.children.insert(path.clone());
                    match entry.file_type() {
                        Ok(kind) if kind.is_dir() => pending.push(path),
                        Ok(kind) if kind.is_file() => {
                            coverage.files.insert(path);
                            if coverage.files.len().is_multiple_of(128) {
                                progress(ScanProgress {
                                    discovered: coverage.files.len(),
                                    completed: 0,
                                    total: None,
                                });
                            }
                        }
                        Ok(_) => {} // Do not follow symbolic links or special files.
                        Err(error) => {
                            evidence.complete = false;
                            coverage.record_io_failure(&path, &error);
                        }
                    }
                    if coverage.source_disconnected {
                        break;
                    }
                }
            }
        }
        coverage.directories.insert(directory, evidence);
        if coverage.source_disconnected {
            break;
        }
    }
    Ok(coverage)
}

impl Coverage {
    fn issue(&mut self, path: &Path, reason: String) {
        if !self
            .issues
            .last()
            .is_some_and(|issue| issue.path == path.to_string_lossy() && issue.reason == reason)
        {
            self.issues.push(ScanIssue {
                path: path.to_string_lossy().into_owned(),
                reason,
            });
        }
    }

    fn directory_error(&mut self, path: &Path, error: &io::Error) {
        self.source_disconnected |= path == self.root || source_error(error);
        self.issue(path, error.to_string());
    }

    fn record_io_failure(&mut self, path: &Path, error: &io::Error) {
        self.source_disconnected |= source_error(error);
        self.failures.insert(path.into(), error.to_string());
        self.issue(path, error.to_string());
    }

    pub fn record_probe_failure(&mut self, path: &Path, error: &anyhow::Error) {
        self.source_disconnected |= source_failure(error);
        let reason = format!("{error:#}");
        self.failures.insert(path.into(), reason.clone());
        self.issue(path, reason);
    }

    /// Call after metadata reads too: a share may disappear after enumeration.
    pub fn check_source(&mut self) {
        self.check_source_with(|path| {
            let identity = source_identity(path)?;
            // Opening an iterator alone can succeed before an I/O error is
            // delivered. Fetch an entry to establish that the root is readable.
            if let Some(entry) = fs::read_dir(path)?.next() {
                entry?;
            }
            Ok(identity)
        });
    }

    fn check_source_with(&mut self, mut check: impl FnMut(&Path) -> io::Result<String>) {
        let mut mounts = self.mounts.clone();
        if let Some(identity) = &self.source_identity {
            mounts
                .entry(self.root.clone())
                .or_insert_with(|| identity.clone());
        }
        for (path, expected) in mounts {
            match check(&path) {
                Ok(identity) if expected == identity => {}
                Ok(_) => {
                    self.source_disconnected = true;
                    self.issue(
                        &path,
                        "The music source changed or disconnected during refresh".into(),
                    );
                }
                Err(error) => {
                    self.source_disconnected = true;
                    self.issue(&path, format!("Couldn’t check the music source: {error}"));
                }
            }
            if self.source_disconnected {
                break;
            }
        }
    }

    /// A missing item can be removed only when a complete parent listing proves
    /// its absence. A deleted ancestor proves the absence of all its descendants.
    pub fn removal_proven(&self, path: &Path) -> bool {
        if self.source_disconnected {
            return false;
        }
        let Ok(relative) = path.strip_prefix(&self.root) else {
            return false;
        };
        if relative
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return false;
        }
        let mut directory = self.root.clone();
        let mut components = relative.components().peekable();
        while let Some(component) = components.next() {
            let child = directory.join(component);
            let Some(coverage) = self.directories.get(&directory) else {
                return false;
            };
            if !coverage.children.contains(&child) {
                return coverage.complete;
            }
            if components.peek().is_none() {
                // A directory now occupies the old audio file's path.
                return self.directories.contains_key(&child);
            }
            if self.files.contains(&child) {
                // A regular file now occupies an old ancestor directory.
                return true;
            }
            directory = child;
        }
        false
    }
}

pub(crate) fn source_failure(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(source_error)
}

fn source_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::TimedOut
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::HostUnreachable
    ) {
        return true;
    }
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error() {
        return [
            libc::EIO,
            libc::ESTALE,
            libc::ENXIO,
            libc::ENODEV,
            libc::ENOTCONN,
            libc::ETIMEDOUT,
            libc::ENETDOWN,
            libc::ENETUNREACH,
            libc::EHOSTUNREACH,
            libc::ECONNRESET,
            libc::ECONNABORTED,
            libc::EPIPE,
        ]
        .contains(&code);
    }
    #[cfg(windows)]
    if let Some(code) = error.raw_os_error() {
        // ERROR_BAD_NETPATH, NETNAME_DELETED, BAD_NET_NAME, SEM_TIMEOUT,
        // NETWORK_UNREACHABLE and HOST_UNREACHABLE.
        return [53, 64, 67, 121, 1231, 1232].contains(&code);
    }
    false
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn source_identity(root: &Path) -> io::Result<String> {
    use std::{
        ffi::{CStr, CString},
        os::unix::ffi::OsStrExt,
    };
    let path = CString::new(root.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Path contains a NUL byte"))?;
    let mut info = std::mem::MaybeUninit::<libc::statfs>::uninit();
    // statfs identifies the mount and remote share name. Local BSD device names
    // are reusable, so local volumes additionally require a persistent UUID.
    let result = unsafe { libc::statfs(path.as_ptr(), info.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let info = unsafe { info.assume_init() };
    let filesystem = unsafe { CStr::from_ptr(info.f_fstypename.as_ptr()) }.to_bytes();
    let source = unsafe { CStr::from_ptr(info.f_mntfromname.as_ptr()) }.to_bytes();
    let mount = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) }.to_bytes();
    if let Some(expected) = expected_volume_mount(root)
        && !Path::new(std::ffi::OsStr::from_bytes(mount)).starts_with(expected)
    {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "The music source is not mounted; no tracks removed",
        ));
    }
    // Encode bytes losslessly: mount names are not required to be UTF-8.
    if info.f_flags & libc::MNT_LOCAL as u32 != 0 {
        let mount_path = unsafe { CStr::from_ptr(info.f_mntonname.as_ptr()) };
        let uuid = local_volume_uuid(mount_path)?;
        Ok(local_identity(filesystem, mount, &uuid))
    } else {
        Ok(format!("apple-network:{filesystem:?}:{source:?}:{mount:?}"))
    }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn local_identity(filesystem: &[u8], mount: &[u8], uuid: &[u8; 16]) -> String {
    format!("apple-volume:{filesystem:?}:{uuid:02x?}:{mount:?}")
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn local_volume_uuid(mount: &std::ffi::CStr) -> io::Result<[u8; 16]> {
    let mut attributes = libc::attrlist {
        bitmapcount: libc::ATTR_BIT_MAP_COUNT,
        reserved: 0,
        commonattr: 0,
        volattr: libc::ATTR_VOL_INFO | libc::ATTR_VOL_UUID,
        dirattr: 0,
        fileattr: 0,
        forkattr: 0,
    };
    // getattrlist packs the returned u32 byte count directly before the UUID.
    // A byte buffer avoids inserting native struct padding at this FFI boundary.
    let mut bytes = [0u8; 20];
    let result = unsafe {
        libc::getattrlist(
            mount.as_ptr(),
            (&mut attributes as *mut libc::attrlist).cast(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            libc::FSOPT_REPORT_FULLSIZE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    parse_volume_uuid(&bytes)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn parse_volume_uuid(bytes: &[u8; 20]) -> io::Result<[u8; 16]> {
    if u32::from_ne_bytes(bytes[..4].try_into().unwrap()) != bytes.len() as u32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Incomplete volume identity",
        ));
    }
    let uuid: [u8; 16] = bytes[4..].try_into().unwrap();
    if uuid.iter().all(|byte| *byte == 0) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "The mounted volume has no persistent identity; no tracks removed",
        ));
    }
    Ok(uuid)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn expected_volume_mount(root: &Path) -> Option<PathBuf> {
    let relative = root.strip_prefix("/Volumes").ok()?;
    let volume = relative.components().next()?;
    if !matches!(volume, std::path::Component::Normal(_)) {
        return None;
    }
    Some(Path::new("/Volumes").join(volume))
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
fn source_identity(root: &Path) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("device:{}", fs::metadata(root)?.dev()))
}

#[cfg(not(unix))]
fn source_identity(root: &Path) -> io::Result<String> {
    Ok(format!("root:{}", root.canonicalize()?.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(root: &Path) -> Coverage {
        enumerate(root, &|| false, &mut |_| {}).unwrap()
    }

    #[test]
    fn complete_parent_proves_deleted_files_and_entire_deleted_subtrees() {
        let root = tempfile::tempdir().unwrap();
        let album = root.path().join("album");
        fs::create_dir(&album).unwrap();
        fs::write(album.join("present.wav"), []).unwrap();
        let coverage = scan(root.path());
        assert!(coverage.removal_proven(&album.join("deleted.wav")));
        assert!(coverage.removal_proven(&root.path().join("deleted-album/song.wav")));
        assert!(!coverage.removal_proven(&album.join("present.wav")));
        assert!(!coverage.removal_proven(&root.path().join("../elsewhere.wav")));
    }

    #[test]
    fn known_type_replacements_prove_old_files_and_descendants_are_gone() {
        let root = tempfile::tempdir().unwrap();
        let file_replaced_by_directory = root.path().join("was-a-song.wav");
        fs::create_dir(&file_replaced_by_directory).unwrap();
        let directory_replaced_by_file = root.path().join("was-an-album");
        fs::write(&directory_replaced_by_file, []).unwrap();
        let mut coverage = scan(root.path());
        assert!(coverage.removal_proven(&file_replaced_by_directory));
        assert!(coverage.removal_proven(&directory_replaced_by_file.join("song.wav")));
        assert!(!coverage.removal_proven(&directory_replaced_by_file));
        coverage.source_disconnected = true;
        assert!(!coverage.removal_proven(&file_replaced_by_directory));
        assert!(!coverage.removal_proven(&directory_replaced_by_file.join("song.wav")));
    }

    #[test]
    fn unreadable_folder_preserves_its_entries_without_blocking_sibling_deletions() {
        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good");
        let blocked = root.path().join("blocked");
        fs::create_dir(&good).unwrap();
        fs::create_dir(&blocked).unwrap();
        fs::write(good.join("new.wav"), []).unwrap();
        let coverage = enumerate_with(root.path(), &|| false, &mut |_| {}, &mut |path| {
            if path == blocked {
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            } else {
                fs::read_dir(path)
            }
        })
        .unwrap();
        assert!(!coverage.source_disconnected);
        assert!(coverage.files.contains(&good.join("new.wav")));
        assert!(coverage.removal_proven(&good.join("deleted.wav")));
        assert!(!coverage.removal_proven(&blocked.join("old.wav")));
        assert_eq!(coverage.issues.len(), 1);
    }

    #[test]
    fn disconnect_or_mount_replacement_cancels_all_deletion_evidence() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("deleted.wav");
        let mut coverage = scan(root.path());
        assert!(coverage.removal_proven(&absent));
        coverage.check_source_with(|_| Ok("a different mounted source".into()));
        assert!(coverage.source_disconnected);
        assert!(!coverage.removal_proven(&absent));
        let mut coverage = scan(root.path());
        coverage.record_probe_failure(
            &root.path().join("song.wav"),
            &io::Error::from(io::ErrorKind::ConnectionReset).into(),
        );
        assert!(coverage.source_disconnected);
        assert!(!coverage.removal_proven(&absent));
    }

    #[test]
    fn bad_metadata_does_not_imply_source_disconnection() {
        let root = tempfile::tempdir().unwrap();
        let mut coverage = scan(root.path());
        coverage.record_probe_failure(
            &root.path().join("song.wav"),
            &anyhow::anyhow!("Malformed audio tag"),
        );
        assert!(!coverage.source_disconnected);
        assert!(coverage.removal_proven(&root.path().join("deleted.wav")));
        coverage.check_source();
        assert!(!coverage.source_disconnected);
    }

    #[test]
    fn nested_mounts_are_recorded_and_a_clean_unmount_invalidates_all_deletions() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("mounted-share");
        fs::create_dir_all(nested.join("album")).unwrap();
        fs::write(nested.join("album/song.wav"), []).unwrap();
        let mut coverage = enumerate_with_identity(
            root.path(),
            &|| false,
            &mut |_| {},
            &mut |path| fs::read_dir(path),
            &mut |path| {
                Ok(if path.starts_with(&nested) {
                    "nas"
                } else {
                    "local"
                }
                .into())
            },
        )
        .unwrap();
        assert_eq!(coverage.mounts.len(), 2);
        assert_eq!(coverage.mounts.get(&nested).unwrap(), "nas");
        assert!(coverage.removal_proven(&root.path().join("deleted.wav")));
        // The old mountpoint remains an accessible local directory after unmount.
        coverage.check_source_with(|_| Ok("local".into()));
        assert!(coverage.source_disconnected);
        assert!(!coverage.removal_proven(&root.path().join("deleted.wav")));
        assert!(!coverage.removal_proven(&nested.join("album/deleted.wav")));
    }

    #[test]
    fn a_legacy_unmounted_child_stops_enumeration_without_losing_discovered_files() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("mounted-share");
        fs::create_dir(&nested).unwrap();
        let discovered = root.path().join("new.wav");
        fs::write(&discovered, []).unwrap();
        let coverage = enumerate_with_identity(
            root.path(),
            &|| false,
            &mut |_| {},
            &mut |path| fs::read_dir(path),
            &mut |path| {
                if path == nested {
                    Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "Share is not mounted",
                    ))
                } else {
                    Ok("local".into())
                }
            },
        )
        .unwrap();
        assert!(coverage.source_disconnected);
        assert!(coverage.files.contains(&discovered));
        assert!(!coverage.directories[&nested].complete);
        assert!(!coverage.removal_proven(&root.path().join("deleted.wav")));
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn local_identity_requires_a_real_uuid_and_distinguishes_reused_disk_devices() {
        let mut bytes = [0u8; 20];
        bytes[..4].copy_from_slice(&20u32.to_ne_bytes());
        assert!(parse_volume_uuid(&bytes).is_err());
        bytes[4..].fill(1);
        let first = parse_volume_uuid(&bytes).unwrap();
        bytes[4..].fill(2);
        let second = parse_volume_uuid(&bytes).unwrap();
        assert_ne!(
            local_identity(b"apfs", b"/Volumes/Music", &first),
            local_identity(b"apfs", b"/Volumes/Music", &second)
        );
        bytes[..4].copy_from_slice(&16u32.to_ne_bytes());
        assert!(parse_volume_uuid(&bytes).is_err());
    }

    #[test]
    fn cancellation_never_returns_a_partial_success_and_symlinks_are_not_followed() {
        let root = tempfile::tempdir().unwrap();
        assert!(enumerate(root.path(), &|| true, &mut |_| {}).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
            let coverage = scan(root.path());
            assert_eq!(coverage.directories.len(), 1);
            assert!(coverage.files.is_empty());
        }
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn only_volume_children_require_a_matching_mountpoint() {
        assert_eq!(expected_volume_mount(Path::new("/Volumes")), None);
        assert_eq!(expected_volume_mount(Path::new("/Users/music")), None);
        assert_eq!(
            expected_volume_mount(Path::new("/Volumes/NAS/albums")),
            Some(PathBuf::from("/Volumes/NAS"))
        );
    }
}
