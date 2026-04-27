//! Stdin → `PathRecord` plumbing for piped invocations.

use std::path::PathBuf;

use anyhow::Result;

use crate::scan::PathRecord;

/// True when stdin is a pipe or redirected regular file. TTY, `/dev/null`,
/// and sockets all return false - `is_terminal()` alone can't distinguish a
/// real pipe from `/dev/null`. Sockets are excluded so IPC test harnesses
/// don't trigger stdin mode.
#[cfg(unix)]
pub(crate) fn stdin_has_input() -> bool {
    use std::fs::File;
    use std::mem::ManuallyDrop;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::fs::FileTypeExt as _;

    let fd = std::io::stdin().as_raw_fd();
    // SAFETY: ManuallyDrop keeps fd 0 open after the borrow.
    let file = ManuallyDrop::new(unsafe { File::from_raw_fd(fd) });
    let Ok(meta) = file.metadata() else {
        return false;
    };
    meta.file_type().is_fifo() || meta.is_file()
}

#[cfg(not(unix))]
pub(crate) fn stdin_has_input() -> bool {
    use std::io::IsTerminal as _;
    !std::io::stdin().is_terminal()
}

/// Read stdin filenames, NUL-separated when `null` is true else newline. On
/// Unix the bytes go straight to `OsStr` so non-UTF-8 names survive.
pub(crate) fn read_paths_from_stdin(null: bool) -> Result<Vec<PathBuf>> {
    use std::io::Read as _;

    let mut input = Vec::new();
    std::io::stdin().lock().read_to_end(&mut input)?;
    let sep = if null { 0u8 } else { b'\n' };
    let mut paths = Vec::new();
    for chunk in input.split(|b| *b == sep) {
        let trimmed = if !null && chunk.last() == Some(&b'\r') {
            &chunk[..chunk.len() - 1]
        } else {
            chunk
        };
        if trimmed.is_empty() {
            continue;
        }
        paths.push(bytes_to_path(trimmed));
    }
    Ok(paths)
}

#[cfg(unix)]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt as _;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

/// `PathRecord`s for caller-supplied paths. `root` is the parent so each
/// entry registers as depth-1 - the user picked the file set, we don't
/// reinterpret it as a tree. Non-existent paths are dropped.
pub(crate) fn records_from_paths(paths: Vec<PathBuf>) -> Vec<PathRecord> {
    paths
        .into_iter()
        .filter(|p| p.exists())
        .map(|p| {
            let root = p
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            PathRecord { path: p, root }
        })
        .collect()
}
