use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};

/// Atomically replace `path` with `bytes`: temp file in the same directory,
/// fsync, rename, fsync the directory.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .context("atomic_write target has no parent directory")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
        }
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("renaming into {}", path.display()));
    }
    fsync_dir(dir);
    Ok(())
}

pub fn fsync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}
