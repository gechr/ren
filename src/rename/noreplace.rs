//! Atomic renames that refuse to replace an existing destination.

use std::io;
use std::path::Path;

#[cfg(unix)]
pub(super) fn entry_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = path.symlink_metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
pub(super) fn entry_identity(path: &Path) -> io::Result<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, CreateFileW, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        GetFileInformationByHandle, OPEN_EXISTING,
    };

    let path: Vec<_> = verbatim(path)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if path[..path.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    // SAFETY: path is a live NUL-terminated string. No access is requested,
    // and OPEN_REPARSE_POINT identifies the link itself, including dangling links.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a valid, uniquely owned handle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut info = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: the handle remains open and info is writable for the whole call.
    if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), info.as_mut_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call initialized every field of info.
    let info = unsafe { info.assume_init() };
    Ok((
        u64::from(info.dwVolumeSerialNumber),
        (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
    ))
}

#[cfg(not(any(unix, windows)))]
pub(super) fn entry_identity(_path: &Path) -> io::Result<(u64, u64)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory entry identity is unavailable on this platform",
    ))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(|error| {
        if matches!(error, rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP) {
            io::Error::new(io::ErrorKind::Unsupported, "filesystem does not support atomic no-overwrite rename")
        } else if error == rustix::io::Errno::INVAL {
            io::Error::new(io::ErrorKind::InvalidInput, "atomic no-overwrite rename rejected - invalid paths or unsupported filesystem operation")
        } else { io::Error::from(error) }
    })
}

#[cfg(windows)]
pub(super) fn rename(from: &Path, to: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let from: Vec<_> = verbatim(from)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let to: Vec<_> = verbatim(to)?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if from[..from.len() - 1].contains(&0) || to[..to.len() - 1].contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    // SAFETY: both pointers refer to live, NUL-terminated UTF-16 strings.
    // Zero flags forbid replacing a destination or falling back to a copy.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(windows)]
fn verbatim(path: &Path) -> io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::path::{Component, Prefix};

    // On Windows absolute() uses GetFullPathNameW, resolving drive-relative
    // paths, separators and dot components before adding the verbatim prefix.
    let absolute = std::path::absolute(path)?;
    let mut components = absolute.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path has no Windows prefix",
        ));
    };
    match prefix.kind() {
        Prefix::Disk(_) => {
            let mut full = OsString::from(r"\\?\");
            full.push(absolute);
            Ok(full.into())
        }
        Prefix::UNC(server, share) => {
            let mut full = std::path::PathBuf::from(r"\\?\UNC\");
            full.push(server);
            full.push(share);
            for component in components {
                if let Component::Normal(name) = component {
                    full.push(name);
                }
            }
            Ok(full)
        }
        _ => Ok(absolute),
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn unc_paths_get_verbatim_prefix() {
        assert_eq!(
            verbatim(Path::new(r"\\server\share\folder\file")).unwrap(),
            Path::new(r"\\?\UNC\server\share\folder\file")
        );
    }

    #[test]
    fn native_rename_accepts_long_paths() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp
            .path()
            .join("a".repeat(120))
            .join("b".repeat(120))
            .join("c".repeat(120));
        std::fs::create_dir_all(&dir).unwrap();
        let old = dir.join("old");
        let new = dir.join("new");
        std::fs::write(&old, "content").unwrap();
        rename(&old, &new).unwrap();
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "content");
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub(super) fn rename(_from: &Path, _to: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "atomic no-overwrite rename is unavailable on this platform",
    ))
}
