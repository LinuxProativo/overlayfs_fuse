//! Main control logic for the OverlayFS lifecycle.
//!
//! This module coordinates the mounting, unmounting, and synchronization
//! (commit/discard) of the overlay layers using FUSE.

use crate::commit_filter::CommitFilter;
use crate::files::OverlayFiles;
use crate::fuse_ops::OverlayOps;
use crate::layers::WH_PREFIX;
use crate::InodeMode;
use fuser::{BackgroundSession, Config, MountOption, SessionACL};
use libc::{lgetxattr, llistxattr, lsetxattr};
use recursive_copy::CopyOptions;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::io::{Error, Result};
use std::os::unix;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;
use std::{fs, ptr};

/// Defines the finalization strategy for the upper layer when the filesystem is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OverlayAction {
    /// Keep the upper layer files as they are.
    Preserve,
    /// Delete the upper layer and working directory.
    Discard,
    /// Merge the upper layer changes back into the lower layer and then cleanup.
    Commit,
    /// Perform an atomic merge using a backup and swap strategy to ensure data integrity.
    CommitAtomic,
}

/// A handle providing read-only access to the paths used by an active overlay.
pub struct OverlayHandle {
    /// Path to the base read-only layer.
    lower_path: PathBuf,
    /// Path where the overlay is currently mounted.
    mount_point: PathBuf,
    /// Path to the read-write upper layer.
    upper_path: PathBuf,
}

impl OverlayHandle {
    /// Returns the path to the mount point.
    ///
    /// # Returns
    /// * A reference to the `Path` where the FS is mounted.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// Returns the path to the upper layer.
    ///
    /// # Returns
    /// * A reference to the `Path` used as the read-write layer.
    pub fn upper(&self) -> &Path {
        &self.upper_path
    }

    /// Returns the path to the lower layer.
    ///
    /// # Returns
    /// * A reference to the `Path` used as the base read-only layer.
    pub fn lower(&self) -> &Path {
        &self.lower_path
    }
}

/// The primary controller for the Overlay Filesystem.
pub struct OverlayFS {
    /// The Inode generation strategy used for this mount.
    mode: InodeMode,
    /// Holds the active FUSE background session.
    session: Option<BackgroundSession>,
    /// Metadata and path configuration for the overlay layers.
    files: OverlayFiles,
    /// Optional filter applied during every commit operation.
    commit_filter: Option<CommitFilter>,
}

impl OverlayFS {
    /// Creates a new `OverlayFS` instance with default path naming conventions.
    ///
    /// # Arguments
    /// * `lower_dir` - The base directory to use as the lower layer.
    ///
    /// # Returns
    /// * A new `OverlayFS` instance with `upper` and `mount` paths derived from `lower_dir`.
    pub fn new(lower_dir: PathBuf) -> Self {
        let files = OverlayFiles::new(lower_dir.clone());

        Self {
            files,
            mode: InodeMode::Virtual,
            session: None,
            commit_filter: None,
        }
    }

    /// Overrides the default upper layer path.
    ///
    /// # Arguments
    /// * `path` - The new `PathBuf` for the upper (read-write) layer.
    ///
    /// # Returns
    /// * A mutable reference to `Self` for method chaining.
    ///
    /// # Panics
    /// If the overlay is already mounted.
    pub fn set_upper(&mut self, path: PathBuf) -> &mut Self {
        assert!(
            !self.is_mounted(),
            "Cannot change upper layer path while the filesystem is mounted"
        );
        self.files.upper = path;
        self
    }

    /// Sets the inode generation mode.
    ///
    /// # Arguments
    /// * `mode` - The `InodeMode` strategy (e.g., `Virtual` or `Persistent`).
    ///
    /// # Returns
    /// * A mutable reference to `Self` for method chaining.
    ///
    /// # Panics
    /// If the overlay is already mounted.
    pub fn set_inode_mode(&mut self, mode: InodeMode) -> &mut Self {
        assert!(
            !self.is_mounted(),
            "Cannot change inode mode while the filesystem is mounted"
        );
        self.mode = mode;
        self
    }

    /// Configures the mount point to be inside the user's home cache directory.
    ///
    /// If the `HOME` environment variable is present, it relocates the mount point
    /// to `~/.cache/mount_name`. Otherwise, it retains its current location.
    ///
    /// # Returns
    /// * A mutable reference to `Self` to allow for method chaining.
    ///
    /// # Panics
    /// If the overlay is already mounted.
    pub fn mountpoint_as_home(&mut self) -> &mut Self {
        assert!(
            !self.is_mounted(),
            "Cannot relocate mount point while the filesystem is mounted"
        );
        self.files.mountpoint_as_home();
        self
    }

    /// Sets a [`CommitFilter`] that is applied whenever the upper layer is merged
    /// into the lower layer (both `Commit` and `CommitAtomic` modes).
    ///
    /// Calling this method replaces any previously configured filter. Pass
    /// [`CommitFilter::new()`] to clear all exclusions, or
    /// [`CommitFilter::rootfs()`] to use the rootfs-appropriate defaults.
    ///
    /// The filter can be set at any time, including while the filesystem is
    /// mounted, since it is only consulted at commit time.
    ///
    /// # Arguments
    /// * `filter` - The [`CommitFilter`] instance to use from now on.
    ///
    /// # Returns
    /// * A mutable reference to `Self` for method chaining.
    pub fn set_commit_filter(&mut self, filter: CommitFilter) -> &mut Self {
        self.commit_filter = Some(filter);
        self
    }

    /// Removes any previously configured [`CommitFilter`], restoring the default
    /// behavior where every entry in the upper layer is committed as-is.
    ///
    /// # Returns
    /// * A mutable reference to `Self` for method chaining.
    pub fn clear_commit_filter(&mut self) -> &mut Self {
        self.commit_filter = None;
        self
    }

    /// Mounts the filesystem using FUSE.
    ///
    /// # Returns
    /// * `Ok(())` if directories are prepared and the FUSE session starts successfully.
    /// * `Err` if directory creation or the mount operation fails.
    pub fn mount(&mut self) -> Result<()> {
        if !self.files.mount_point.exists() {
            fs::create_dir_all(&self.files.mount_point)?;
        }
        if !self.files.upper.exists() {
            fs::create_dir_all(&self.files.upper)?;
        }

        let backend = OverlayFiles {
            lower: self.files.lower.clone(),
            upper: self.files.upper.clone(),
            mount_point: self.files.mount_point.clone(),
        };

        let ops = OverlayOps::new(backend, self.mode);

        let mut config = Config::default();
        config.acl = SessionACL::Owner;

        let mut opts: Vec<MountOption> = Vec::<MountOption>::new();
        opts.push(MountOption::FSName("overlay_fuse".to_string()));
        opts.push(MountOption::RW);
        config.mount_options = opts;

        let session = fuser::spawn_mount2(ops, &self.files.mount_point, &config)?;
        self.session = Some(session);
        Ok(())
    }

    /// Returns a handle containing the paths for this filesystem instance.
    ///
    /// # Returns
    /// * An `OverlayHandle` with cloned path information.
    pub fn handle(&self) -> OverlayHandle {
        OverlayHandle {
            lower_path: self.files.lower.clone(),
            mount_point: self.files.mount_point.clone(),
            upper_path: self.files.upper.clone(),
        }
    }

    /// Attempts to unmount the filesystem gracefully.
    ///
    /// It drops the FUSE session and uses manual unmount flags if the filesystem stays busy.
    pub fn umount(&mut self) {
        if let Some(session) = self.session.take() {
            drop(session);
            for _ in 0..10 {
                if !self.is_mounted() {
                    break;
                }
                sleep(Duration::from_millis(100));
            }
        }

        if self.is_mounted() {
            self.internal_libc_umount(false);

            if self.is_mounted() {
                self.internal_libc_umount(true);
            }
        }

        if self.files.mount_point.exists() {
            let _ = fs::remove_dir_all(&self.files.mount_point);
        }
    }

    /// Internal helper to perform unmount via libc syscalls.
    ///
    /// # Arguments
    /// * `lazy` - If `true`, performs a detached (lazy) unmount (`MNT_DETACH`).
    fn internal_libc_umount(&self, lazy: bool) {
        if let Ok(path_c) = CString::new(self.files.mount_point.as_os_str().as_bytes()) {
            let flags = if lazy { libc::MNT_DETACH } else { 0 };
            unsafe {
                libc::umount2(path_c.as_ptr(), flags);
            }
        }
    }

    /// Checks if the mount point is actually a mounted filesystem.
    ///
    /// # Returns
    /// * `true` if the device ID of the mount point differs from its parent.
    fn is_mounted(&self) -> bool {
        if self.files.mount_point == Path::new("/") {
            return true;
        }

        let Ok(m1) = fs::metadata(&self.files.mount_point) else {
            return false;
        };

        let parent = self
            .files
            .mount_point
            .parent()
            .unwrap_or(&self.files.mount_point);
        let Ok(m2) = fs::metadata(parent) else {
            return true;
        };
        m1.dev() != m2.dev()
    }

    /// Executes a finalization action on the overlay layers.
    ///
    /// # Arguments
    /// * `action` - The `OverlayAction` determining if changes should be kept, deleted, or merged.
    pub fn overlay_action(&mut self, action: OverlayAction) {
        if self.session.is_some() {
            self.umount();
        }

        match action {
            OverlayAction::Preserve => (),
            OverlayAction::Discard => {
                let _ = obliterate::ensure_removed(&self.files.upper);
            }
            OverlayAction::Commit => {
                if let Err(e) = self.commit_changes(&self.files.upper, &self.files.lower) {
                    eprintln!("Failed to commit changes: {}", e);
                } else {
                    let _ = obliterate::ensure_removed(&self.files.upper);
                }
            }
            OverlayAction::CommitAtomic => {
                if let Err(e) = self.commit_atomic(&self.files.upper, &self.files.lower) {
                    eprintln!("Failed to commit atomic changes: {}", e);
                } else {
                    let _ = obliterate::ensure_removed(&self.files.upper);
                }
            }
        }
    }

    /// Recursively merges changes from a source directory into a destination directory.
    ///
    /// Uses a two-phase approach to avoid leaving lower in an inconsistent state if
    /// an error occurs mid-way: first all files are copied, then the source is removed.
    /// Whiteout files are processed to delete the corresponding entry from lower.
    /// File copies preserve permissions via `fs::set_permissions`.
    ///
    /// # Arguments
    /// * `src` - Source directory (upper layer).
    /// * `dst` - Destination directory (lower layer).
    ///
    /// # Returns
    /// * `Ok(())` if the commit finishes without IO errors.
    /// * `Err` if any copy or delete operation fails.
    fn commit_changes(&self, src: &Path, dst: &Path) -> Result<()> {
        if !src.exists() {
            return Ok(());
        }

        self.commit_copy_phase(src, dst)?;

        {
            let dir = fs::File::open(dst)?;
            dir.sync_all()?;
        }

        Ok(())
    }

    /// Performs an atomic commit by preparing a new tree and swapping it with the original.
    ///
    /// The strategy is:
    /// 1. Clone the current lower into `lower.new` (filtered by [`CommitFilter`] if set,
    ///    so virtual/bind-mount directories are not uselessly duplicated).
    /// 2. Merge the upper layer changes into `lower.new`.
    /// 3. Rename the current lower → `lower.backup`, then `lower.new` → lower.
    /// 4. Remove the backup on success (or restore it on failure).
    ///
    /// # Arguments
    /// * `src` - Source directory containing the new changes (upper).
    /// * `dst` - Destination directory to be updated (lower).
    ///
    /// # Returns
    /// * `Ok(())` if the atomic swap is successful.
    /// * `Err` if copying, metadata sync, or renaming fails.
    fn commit_atomic(&self, src: &Path, dst: &Path) -> Result<()> {
        if !src.exists() {
            return Ok(());
        }

        let new_lower = dst.with_extension("new");
        let backup_lower = dst.with_extension("backup");

        if new_lower.exists() {
            obliterate::ensure_removed(&new_lower).ok();
        }
        if backup_lower.exists() {
            obliterate::ensure_removed(&backup_lower).ok();
        }

        recursive_copy::copy_recursive(dst, &new_lower, &CopyOptions::default())
            .map_err(|e| Error::new(std::io::ErrorKind::Other, e.to_string()))?;

        if let Err(e) = self.commit_copy_phase(src, &new_lower) {
            obliterate::ensure_removed(&new_lower).ok();
            return Err(e);
        }

        {
            let dir = fs::File::open(&new_lower)?;
            dir.sync_all()?;
        }

        fs::rename(dst, &backup_lower)?;

        if let Err(e) = fs::rename(&new_lower, dst) {
            fs::rename(&backup_lower, dst).ok();
            return Err(e);
        }

        obliterate::ensure_removed(&backup_lower).ok();
        Ok(())
    }

    /// Recursively copies an entire directory tree from source to destination.
    ///
    /// # Arguments
    /// * `src` - The source directory path.
    /// * `dst` - The destination directory path.
    ///
    /// # Returns
    /// * `Ok(())` if the entire tree is copied successfully.
    /// * `Err` if any I/O error occurs during traversal or copying.
    // fn copy_tree(&self, src: &Path, dst: &Path) -> Result<()> {
    //     if !dst.exists() {
    //         fs::create_dir_all(dst)?;
    //     }
    //
    //     let mut options = CopyOptions::new();
    //     options.content_only = true;
    //     options.overwrite = true;
    //
    //     copy(src, dst, &options).map_err(|e| {
    //         Error::new(
    //             std::io::ErrorKind::Other,
    //             format!("fs_extra error: {} (src: {:?}, dst: {:?})", e, src, dst),
    //         )
    //     })?;
    //
    //     Ok(())
    // }

    /// Synchronizes changes from the upper layer back to the lower layer.
    ///
    /// This function performs an iterative tree traversal to merge the filesystem state.
    /// It handles file promotion, directory creation, metadata preservation (UID/GID/Permissions),
    /// and processes "whiteout" files to reflect deletions in the final merged state.
    ///
    /// If a [`CommitFilter`] is configured, matching entries are silently skipped
    /// *before* any I/O is attempted, so neither the entry nor its whiteout
    /// counterpart reaches the destination.  Whiteout processing always runs
    /// first so that the filter never accidentally suppresses explicit deletions
    /// in the upper layer.
    ///
    /// # Safety
    /// This function uses `libc::lchown` via `unsafe` blocks to ensure that ownership
    /// is preserved for the current user/group, which is critical for maintaining
    /// environment consistency in tools like `proot` or `bwrap`.
    ///
    /// # Arguments
    /// * `src` - The source path (typically the `upper` directory).
    /// * `dst` - The destination path (typically the `lower` directory).
    ///
    /// # Returns
    /// * `Ok(())` if the merge completes successfully.
    /// * `Err` if any I/O operation or metadata update fails.
    fn commit_copy_phase(&self, src: &Path, dst: &Path) -> Result<()> {
        let src_root = src;
        let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];

        while let Some((current_src, current_dst)) = stack.pop() {
            for entry in fs::read_dir(&current_src)? {
                let entry = entry?;
                let name = entry.file_name();
                let src_path = entry.path();
                let dst_path = current_dst.join(&name);
                let ft = entry.file_type()?;

                if name.to_string_lossy().starts_with(WH_PREFIX) {
                    let target_name = name.to_string_lossy().replacen(WH_PREFIX, "", 1);
                    let target_path = current_dst.join(target_name);
                    let _ = if target_path.is_dir() {
                        fs::remove_dir_all(&target_path)
                    } else {
                        fs::remove_file(&target_path)
                    };
                    continue;
                }

                if let Some(filter) = &self.commit_filter {
                    let rel = src_path.strip_prefix(src_root).unwrap_or(&src_path);
                    if filter.should_skip(rel, &src_path) {
                        continue;
                    }
                    if ft.is_dir() && filter.is_skipped_dir(rel) {
                        continue;
                    }
                }

                if ft.is_dir() {
                    if !dst_path.exists() {
                        fs::create_dir_all(&dst_path)?;
                    }
                    self.sync_metadata(&src_path, &dst_path)?;
                    stack.push((src_path, dst_path));
                } else if ft.is_symlink() {
                    let target = fs::read_link(&src_path)?;
                    if dst_path.exists() {
                        let _ = fs::remove_file(&dst_path);
                    }
                    unix::fs::symlink(target, &dst_path)?;
                    self.sync_metadata(&src_path, &dst_path)?;
                } else {
                    self.copy_if_different(&src_path, &dst_path)?;
                }
            }
        }
        Ok(())
    }

    /// Synchronizes permissions, ownership (UID/GID), and extended attributes.
    ///
    /// # Arguments
    /// * `src` - Path to the source file or directory.
    /// * `dst` - Path to the destination file or directory.
    ///
    /// # Returns
    /// * `Ok(())` if metadata is successfully synchronized.
    /// * `Err` if `lchown` or extended attribute operations fail.
    fn sync_metadata(&self, src: &Path, dst: &Path) -> Result<()> {
        let meta = fs::symlink_metadata(src)?;

        fs::set_permissions(dst, meta.permissions())?;

        let uid = meta.uid();
        let gid = meta.gid();
        if let Ok(c_dst) = CString::new(dst.as_os_str().as_bytes()) {
            unsafe {
                libc::lchown(c_dst.as_ptr(), uid, gid);
            }
        }

        Self::copy_xattrs(src, dst)?;
        Ok(())
    }

    /// Copies all extended attributes (xattrs) from one file to another.
    ///
    /// # Arguments
    /// * `src` - Path to the source entry.
    /// * `dst` - Path to the destination entry.
    ///
    /// # Returns
    /// * `Ok(())` if xattrs are successfully copied or if none are present.
    /// * `Err` if memory allocation or system calls fail.
    pub fn copy_xattrs(src: &Path, dst: &Path) -> Result<()> {
        let src_c = CString::new(src.as_os_str().as_bytes())?;
        let dst_c = CString::new(dst.as_os_str().as_bytes())?;

        unsafe {
            let size = llistxattr(src_c.as_ptr(), ptr::null_mut(), 0);
            if size <= 0 {
                return Ok(());
            }

            let mut buf = vec![0u8; size as usize];
            let size = llistxattr(src_c.as_ptr(), buf.as_mut_ptr() as *mut i8, buf.len());

            let mut start = 0;
            while start < size as usize {
                let Some(end) = buf[start..].iter().position(|&b| b == 0) else {
                    break;
                };

                let name = &buf[start..start + end];
                let name_c = CString::new(name)?;

                let value_size = lgetxattr(src_c.as_ptr(), name_c.as_ptr(), ptr::null_mut(), 0);
                if value_size > 0 {
                    let mut value = vec![0u8; value_size as usize];
                    let actual = lgetxattr(
                        src_c.as_ptr(),
                        name_c.as_ptr(),
                        value.as_mut_ptr() as *mut _,
                        value.len(),
                    );
                    if actual < 0 {
                        let errno = *libc::__errno_location();
                        if errno == libc::ERANGE {
                            let retry_size =
                                lgetxattr(src_c.as_ptr(), name_c.as_ptr(), ptr::null_mut(), 0);
                            if retry_size > 0 {
                                let mut value2 = vec![0u8; retry_size as usize];
                                let n = lgetxattr(
                                    src_c.as_ptr(),
                                    name_c.as_ptr(),
                                    value2.as_mut_ptr() as *mut _,
                                    value2.len(),
                                );
                                if n > 0 {
                                    Self::check_lsetxattr(&dst_c, &name_c, &value)?;
                                }
                            }
                        }
                    } else {
                        Self::check_lsetxattr(&dst_c, &name_c, &value)?;
                    }
                }

                start += end + 1;
            }
        }

        Ok(())
    }

    /// Computes the BLAKE3 hash of a file's content.
    ///
    /// # Arguments
    /// * `path` - Path to the file to be hashed.
    ///
    /// # Returns
    /// * `Ok(Hash)` containing the resulting BLAKE3 hash.
    /// * `Err` if the file cannot be opened or read.
    fn file_hash(path: &Path) -> Result<blake3::Hash> {
        use std::io::Read;

        let mut file = fs::File::open(path)?;
        let mut hasher = blake3::Hasher::new();

        let mut buffer = [0u8; 8192];

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        Ok(hasher.finalize())
    }

    /// Compares two files to determine if their content is identical.
    ///
    /// It first compares metadata (size and modification time) before falling back
    /// to a cryptographic hash check if the modification times differ.
    ///
    /// # Arguments
    /// * `src` - Path to the first file.
    /// * `dst` - Path to the second file.
    ///
    /// # Returns
    /// * `Ok(true)` if files are equal, `Ok(false)` otherwise.
    /// * `Err` if metadata or content cannot be accessed.
    fn files_are_equal(&self, src: &Path, dst: &Path) -> Result<bool> {
        if !dst.exists() {
            return Ok(false);
        }

        let src_meta = fs::metadata(src)?;
        let dst_meta = fs::metadata(dst)?;

        if src_meta.len() != dst_meta.len() {
            return Ok(false);
        }

        if src_meta.mtime() == dst_meta.mtime() {
            return Ok(true);
        }

        let src_hash = Self::file_hash(src)?;
        let dst_hash = Self::file_hash(dst)?;

        Ok(src_hash == dst_hash)
    }

    /// Copies a file from source to destination only if they are different.
    ///
    /// If the files match, only metadata is updated. If they differ, the destination
    /// is replaced by the source content and the new file is `fsync`-ed to guarantee
    /// durability before `commit_changes` removes the upper layer.
    ///
    /// This per-file sync is critical when the lower layer lives on a loop device
    /// (common with proot/bwrap AppImage setups): without it, a crash after
    /// `fs::copy` but before the kernel flushes its page-cache could leave the
    /// lower layer with zero-byte or partially-written files.
    ///
    /// # Arguments
    /// * `src` - Source file path.
    /// * `dst` - Destination file path.
    ///
    /// # Returns
    /// * `Ok(())` on success.
    /// * `Err` if the comparison, copy, or sync operation fails.
    fn copy_if_different(&self, src: &Path, dst: &Path) -> Result<()> {
        if self.files_are_equal(src, dst)? {
            self.sync_metadata(src, dst)?;
            return Ok(());
        }

        if dst.exists() {
            fs::remove_file(dst)?;
        }

        fs::copy(src, dst)?;
        self.sync_metadata(src, dst)?;

        if let Ok(f) = fs::OpenOptions::new().write(true).open(dst) {
            let _ = f.sync_data();
        }

        Ok(())
    }

    /// Safely sets an extended attribute on a file or directory without following symbolic links.
    ///
    /// This is a wrapper around the system's `lsetxattr` call.
    ///
    /// # Arguments
    /// * `path` - The C-compatible string representing the file path.
    /// * `name` - The name of the extended attribute to set.
    /// * `value` - The byte slice containing the attribute data.
    ///
    /// # Returns
    /// * `Result<()>` - `Ok(())` if the attribute was successfully set, or an `io::Error` on failure.
    fn check_lsetxattr(path: &CString, name: &CString, value: &[u8]) -> Result<()> {
        let ret = unsafe {
            lsetxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr() as *const _,
                value.len(),
                0,
            )
        };

        if ret != 0 {
            return Err(Error::last_os_error());
        }

        Ok(())
    }
}

impl Drop for OverlayFS {
    /// Automatically handles resource cleanup and session termination.
    fn drop(&mut self) {
        if self.session.is_some() {
            self.umount();
        }
    }
}
