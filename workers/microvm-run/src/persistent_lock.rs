//! Advisory exclusive lock on the persistent-store image, held for the VM's
//! lifetime so two concurrent launchers can never mount the same RW ext4
//! (page-cache → corruption). Fail-closed: a busy lock aborts the boot.
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

/// RAII guard: the held `File`'s fd carries the `flock`. Dropping it (or process
/// exit) releases the lock.
pub struct PersistentImageLock {
    _file: File,
}

/// Open `path` and take a non-blocking exclusive `flock`. `Err(WouldBlock)` when
/// another process already holds it.
pub fn acquire(path: &Path) -> io::Result<PersistentImageLock> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    // SAFETY: valid fd from the open File; LOCK_EX|LOCK_NB is a pure advisory op.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PersistentImageLock { _file: file })
}

#[cfg(unix)]
#[cfg(test)]
mod tests {
    use super::acquire;

    #[test]
    fn second_acquire_on_same_path_fails_closed() {
        let dir = std::env::temp_dir().join(format!("kastellan-persistlock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let img = dir.join("store.ext4");
        std::fs::write(&img, b"x").unwrap();
        let _held = acquire(&img).expect("first acquire succeeds");
        let second = acquire(&img);
        assert!(second.is_err(), "second concurrent flock must fail closed");
        std::fs::remove_dir_all(&dir).ok();
    }
}
