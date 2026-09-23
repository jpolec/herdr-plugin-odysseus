use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Exclusive advisory lock (`flock`) released on drop. Held only for short
/// read-modify-write critical sections, never while waiting on agents.
#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            loop {
                let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
                if rc == 0 {
                    break;
                }
                let err = std::io::Error::last_os_error();
                if err.kind() != std::io::ErrorKind::Interrupted {
                    return Err(err).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
        })
    }

    /// Non-blocking attempt; `Ok(None)` when another process holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                    return Ok(None);
                }
                return Err(err).with_context(|| format!("locking {}", path.display()));
            }
        }
        Ok(Some(Self {
            file,
            path: path.to_path_buf(),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

fn open(path: &Path) -> Result<File> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening lock file {}", path.display()))
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            unsafe {
                libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn try_acquire_is_exclusive_across_handles() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let a = FileLock::try_acquire(&p).unwrap();
        assert!(a.is_some());
        // flock locks are per open file description, so a second open in the
        // same process is refused too.
        let b = FileLock::try_acquire(&p).unwrap();
        assert!(b.is_none());
        drop(a);
        assert!(FileLock::try_acquire(&p).unwrap().is_some());
    }
}
