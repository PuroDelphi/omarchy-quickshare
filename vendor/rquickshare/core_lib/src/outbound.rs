//! Selected outgoing files retain the descriptor opened at selection time.
use anyhow::{anyhow, ensure, Context, Result};
use std::ffi::CString;
use std::fs::File;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// A regular file securely opened once, with captured display name and size.
///
/// Symlinks anywhere in the selection path are rejected. Renames, unlinking,
/// and path replacement do not retarget this descriptor. Existing writers can
/// still modify its inode; this is not an immutable snapshot of the bytes.
#[derive(Debug)]
pub struct OutboundFile {
    file: File,
    name: String,
    size: u64,
}

impl OutboundFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        ensure!(path.is_absolute(), "send path must be absolute");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("send filename must be UTF-8"))?;
        crate::storage::validate_name(name)?;
        let path_bytes = CString::new(path.as_os_str().as_bytes())?;
        // Linux openat2's extensible ABI. No path canonicalization or fallback:
        // resolution and opening must reject all symlinks in one operation.
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        const RESOLVE_NO_SYMLINKS: u64 = 0x04;
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK) as u64,
            mode: 0,
            resolve: RESOLVE_NO_SYMLINKS,
        };
        let fd = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                libc::AT_FDCWD,
                path_bytes.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot securely open outgoing file (symlinks are not allowed)");
        }
        let file = unsafe { File::from_raw_fd(fd as libc::c_int) };
        let metadata = file.metadata()?;
        ensure!(metadata.is_file(), "send path is not a regular file");
        let size = metadata.len();
        ensure!(size <= crate::storage::MAX_TOTAL_BYTES, "file exceeds size limit");
        Ok(Self { file, name: name.to_owned(), size })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn into_parts(self) -> (File, String, u64) {
        (self.file, self.name, self.size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;

    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!("quickshare-outbound-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn selected_descriptor_survives_rename_unlink_and_private_symlink_replacement() {
        let directory = Directory::new();
        let path = directory.0.join("selected.txt");
        let private = directory.0.join("private.txt");
        let original = b"Only these selected bytes may be read";
        std::fs::write(&path, original).unwrap();
        std::fs::write(&private, b"PRIVATE: must never be read").unwrap();
        let selected = OutboundFile::open(&path).unwrap();
        assert_eq!(selected.name(), "selected.txt");
        assert_eq!(selected.size(), original.len() as u64);
        let moved = directory.0.join("moved.txt");
        std::fs::rename(&path, &moved).unwrap();
        std::fs::remove_file(moved).unwrap();
        symlink(&private, &path).unwrap();
        let (mut file, _, _) = selected.into_parts();
        let mut received = Vec::new();
        file.read_to_end(&mut received).unwrap();
        assert_eq!(received, original);
        assert!(OutboundFile::open(&path).is_err());
    }

    #[test]
    fn ancestor_symlink_substitution_is_rejected() {
        let directory = Directory::new();
        let parent = directory.0.join("selection");
        let private = directory.0.join("private");
        std::fs::create_dir(&parent).unwrap();
        std::fs::create_dir(&private).unwrap();
        std::fs::write(parent.join("data.bin"), b"selected").unwrap();
        std::fs::write(private.join("data.bin"), b"PRIVATE").unwrap();
        let selected = OutboundFile::open(parent.join("data.bin")).unwrap();
        std::fs::rename(&parent, directory.0.join("original")).unwrap();
        symlink(&private, &parent).unwrap();
        assert!(OutboundFile::open(parent.join("data.bin")).is_err());
        let (mut file, _, _) = selected.into_parts();
        let mut received = Vec::new();
        file.read_to_end(&mut received).unwrap();
        assert_eq!(received, b"selected");
    }

    #[test]
    fn directories_devices_and_fifos_are_rejected() {
        let directory = Directory::new();
        assert!(OutboundFile::open(&directory.0).is_err());
        assert!(OutboundFile::open("/dev/null").is_err());
        let fifo = directory.0.join("fifo");
        let path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        // O_NONBLOCK makes opening the FIFO return without waiting for a writer.
        assert!(OutboundFile::open(fifo).is_err());
    }
    #[test]
    fn failed_selection_drops_previously_acquired_descriptors() {
        use std::os::fd::AsRawFd;
        let directory = Directory::new();
        let path = directory.0.join("selected.txt");
        let invalid = directory.0.join("invalid.txt");
        std::fs::write(&path, b"selected").unwrap();
        symlink(&path, &invalid).unwrap();
        let observer = File::open(&path).unwrap();
        let selection: Result<Vec<_>> = [&path, &invalid].into_iter().map(|path| {
            let selected = OutboundFile::open(path)?;
            assert_eq!(unsafe {
                libc::flock(selected.file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
            }, 0);
            assert_eq!(unsafe {
                libc::flock(observer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
            }, -1);
            Ok(selected)
        }).collect();
        assert!(selection.is_err());
        // The first selected descriptor owned the lock. Partial collection
        // failure must close it, not leak it while returning the second error.
        assert_eq!(unsafe {
            libc::flock(observer.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB)
        }, 0);
    }

}
