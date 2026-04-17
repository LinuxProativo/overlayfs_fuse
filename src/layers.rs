//! Layer management and path resolution for the OverlayFS.
//!
//! This module handles the logic for stacking layers, performing Copy-on-Write (CoW)
//! operations, and managing "whiteout" files used to mark deletions in the lower layer.

use crate::files::OverlayFiles;
use crate::OverlayFS;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Prefix used to identify whiteout files in the upper layer.
pub const WH_PREFIX: &str = ".wh.";

/// Manages filesystem layers and coordinates operations between upper and lower directories.
pub struct LayerManager {
    /// The backend providing access to the underlying layer paths.
    pub backend: OverlayFiles,
}

impl LayerManager {
    /// Creates a new `LayerManager` instance.
    ///
    /// # Arguments
    /// * `backend` - The `OverlayFiles` instance containing layer configurations.
    ///
    /// # Returns
    /// * A new `LayerManager` instance.
    pub fn new(backend: OverlayFiles) -> Self {
        Self { backend }
    }

    /// Resolves a relative path to its physical location in the filesystem.
    ///
    /// Use `symlink_metadata` instead of `exists()` so that dangling symlinks
    /// in the upper layer are correctly found rather than falling through to lower.
    ///
    /// # Arguments
    /// * `rel` - The relative path within the overlay to resolve.
    ///
    /// # Returns
    /// * A tuple of the resolved `PathBuf` and `true` if it's in the upper layer.
    pub fn resolve(&self, rel: &Path) -> Option<(PathBuf, bool)> {
        if self.is_hidden(rel) {
            return None;
        }

        let upper = self.backend.upper.join(rel);
        if fs::symlink_metadata(&upper).is_ok() {
            return Some((upper, true));
        }

        let lower = self.backend.lower.join(rel);
        if fs::symlink_metadata(&lower).is_ok() {
            return Some((lower, false));
        }

        None
    }

    /// Checks if a file or directory is hidden by a whiteout file in the upper layer.
    ///
    /// # Arguments
    /// * `rel` - The relative path to check for a whiteout marker.
    ///
    /// # Returns
    /// * `true` if a corresponding whiteout file exists, `false` otherwise.
    pub fn is_hidden(&self, rel: &Path) -> bool {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            let wh = self.backend.upper.join(parent).join(format!(
                "{}{}",
                WH_PREFIX,
                name.to_string_lossy()
            ));
            return fs::symlink_metadata(&wh).is_ok();
        }
        false
    }

    /// Creates a whiteout file in the upper layer to mask an entry from the lower layer.
    ///
    /// # Arguments
    /// * `rel` - The relative path of the entry to be masked.
    ///
    /// # Returns
    /// * `Ok(())` if the whiteout was created successfully.
    /// * `Err` if directory creation or file creation fails.
    pub fn create_whiteout(&self, rel: &Path) -> std::io::Result<()> {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            let wh = self.backend.upper.join(parent).join(format!(
                "{}{}",
                WH_PREFIX,
                name.to_string_lossy()
            ));
            if let Some(p) = wh.parent() {
                fs::create_dir_all(p)?;
            }
            fs::File::create(wh)?;
        }
        Ok(())
    }

    /// Removes a whiteout file from the upper layer if it exists.
    ///
    /// # Arguments
    /// * `rel` - The relative path of the entry whose whiteout should be cleared.
    pub fn clear_whiteout(&self, rel: &Path) {
        if let (Some(parent), Some(name)) = (rel.parent(), rel.file_name()) {
            let wh = self.backend.upper.join(parent).join(format!(
                "{}{}",
                WH_PREFIX,
                name.to_string_lossy()
            ));
            let _ = fs::remove_file(wh);
        }
    }

    /// Performs a Copy-on-Write operation, promoting an entry from lower to upper layer.
    ///
    /// Handles regular files, symlinks, and directories:
    /// - Regular files: copied with `fs::copy`, preserving content.
    /// - Symlinks: recreated with `std::os::unix::fs::symlink` (never followed).
    /// - Directories: created empty in upper; contents are promoted lazily on demand.
    ///
    /// # Arguments
    /// * `rel` - The relative path of the entry to be promoted.
    ///
    /// # Returns
    /// * `Ok(PathBuf)` containing the path in the upper layer.
    /// * `Err` if any directory creation, copy, or symlink operation fails.
    pub fn copy_on_write(&self, rel: &Path) -> std::io::Result<PathBuf> {
        let Some((path, is_upper)) = self.resolve(rel) else {
            return Err(std::io::Error::from_raw_os_error(libc::ENOENT));
        };

        if is_upper {
            return Ok(path);
        }

        let upper_path = self.backend.upper.join(rel);

        if let Some(p) = upper_path.parent() {
            fs::create_dir_all(p)?;
        }

        let meta = fs::symlink_metadata(&path)?;

        if meta.file_type().is_symlink() {
            let target = fs::read_link(&path)?;
            std::os::unix::fs::symlink(target, &upper_path)?;
        } else if meta.is_dir() {
            fs::create_dir_all(&upper_path)?;
        } else {
            fs::copy(&path, &upper_path)?;
        }

        fs::set_permissions(&upper_path, meta.permissions())?;

        let times = [
            libc::timespec {
                tv_sec:  meta.atime() as _, // FIX libc::time_m 32bit
                tv_nsec: meta.atime_nsec() as libc::c_long,
            },
            libc::timespec {
                tv_sec:  meta.mtime() as _, // FIX libc::time_m 32bit
                tv_nsec: meta.mtime_nsec() as libc::c_long,
            },
        ];
        let path_c = std::ffi::CString::new(upper_path.as_os_str().as_bytes())?;
        unsafe {
            libc::utimensat(libc::AT_FDCWD, path_c.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW);
        }

        let uid = meta.uid();
        let gid = meta.gid();

        use std::os::unix::ffi::OsStrExt;
        let path_c = std::ffi::CString::new(upper_path.as_os_str().as_bytes())?;

        unsafe {
            libc::lchown(path_c.as_ptr(), uid, gid);
        }

        OverlayFS::copy_xattrs(&path, &upper_path)?;

        Ok(upper_path)
    }
}
