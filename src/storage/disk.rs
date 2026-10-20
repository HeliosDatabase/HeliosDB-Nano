//! Filesystem capacity queries.
//!
//! One place that answers "how much room is left", so the engine's write-path
//! guard and the coming WAL archiver measure free space the same way, with a
//! syscall instead of a `df` child process.

use std::io;
use std::path::Path;

/// Free bytes available to an unprivileged writer on the filesystem holding
/// `path`.
///
/// `statvfs`'s `f_bavail * f_frsize`: the blocks a non-root process may still
/// allocate, which excludes the root-reserved share (`f_bfree` does not) and is
/// the figure `df --output=avail` prints. `f_frsize` is the unit `f_bavail` is
/// counted in; a filesystem that reports it as 0 is measured in `f_bsize`
/// instead, as POSIX allowed before `f_frsize` existed.
#[cfg(unix)]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call and
    // `stat` is writable storage of exactly the type `statvfs` fills in.
    if unsafe { libc::statvfs(c_path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `statvfs` returned 0, so it initialised every field of `stat`.
    let stat = unsafe { stat.assume_init() };
    // The field widths differ by platform (`fsblkcnt_t` is 32-bit on macOS,
    // 64-bit on Linux), hence the widening and the saturating product.
    let fragment = if stat.f_frsize == 0 {
        stat.f_bsize
    } else {
        stat.f_frsize
    };
    Ok(u64::from(stat.f_bavail).saturating_mul(u64::from(fragment)))
}

/// Without `statvfs` there is no portable measurement; callers treat the error
/// as "unknown" rather than "full".
#[cfg(not(unix))]
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let _ = path;
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "free-space measurement requires statvfs",
    ))
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_writable_directory_reports_free_space() {
        let dir = tempfile::tempdir().expect("tempdir");
        let free = free_bytes(dir.path()).expect("statvfs on a fresh tempdir");
        assert!(free > 0, "a writable tempdir reported 0 free bytes");
    }

    #[test]
    fn a_missing_path_is_an_error_not_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        let err = free_bytes(&missing).expect_err("statvfs on a missing path must fail");
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
    }

    #[test]
    fn a_nul_byte_in_the_path_is_rejected_before_the_syscall() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"bad\0path"));
        let err = free_bytes(path).expect_err("a NUL byte cannot reach statvfs");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput, "{err}");
    }
}
