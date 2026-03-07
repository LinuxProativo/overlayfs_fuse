//! Implementation of FUSE operations for the OverlayFS.
//!
//! This module translates high-level filesystem requests (lookup, read, write, etc.)
//! into layer-aware operations, managing the coordination between the upper
//! (read-write) and lower (read-only) layers.

use crate::files::OverlayFiles;
use crate::inode::{InodeMode, InodeStore};
use crate::layers::{LayerManager, WH_PREFIX};
use fuser::{
    AccessFlags, BsdFileFlags, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, LockOwner, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyLseek,
    ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request, TimeOrNow, WriteFlags,
};
use std::collections::HashSet;
use std::ffi::{CString, OsStr};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// TTL for upper-layer entries: always revalidate since the upper layer is mutable.
const TTL: Duration = Duration::ZERO;

/// Provides the core logic for FUSE callbacks, managing inodes and layer resolution.
pub struct OverlayOps {
    /// Storage and mapping for virtual and persistent inodes.
    pub inodes: InodeStore,
    /// Manager responsible for layer resolution and Copy-on-Write (CoW) logic.
    pub layers: LayerManager,
}

impl OverlayOps {
    /// Creates a new `OverlayOps` instance.
    ///
    /// # Arguments
    /// * `backend` - Configuration containing the paths for lower, upper, and work directories.
    /// * `mode` - The strategy for inode generation (Virtual or Persistent).
    ///
    /// # Returns
    /// * A new instance of `OverlayOps`.
    pub fn new(backend: OverlayFiles, mode: InodeMode) -> Self {
        Self {
            inodes: InodeStore::new(mode),
            layers: LayerManager::new(backend),
        }
    }

    /// Builds a FUSE `FileAttr` from an inode and host filesystem metadata.
    ///
    /// # Arguments
    /// * `ino` - The inode number to assign to the attributes.
    /// * `meta` - Metadata from the underlying host filesystem.
    ///
    /// # Returns
    /// * A `FileAttr` structure compatible with the FUSE protocol.
    fn make_attr(&self, ino: INodeNo, meta: &fs::Metadata) -> FileAttr {
        let ft = meta.file_type();
        let kind = if ft.is_dir() {
            FileType::Directory
        } else if ft.is_symlink() {
            FileType::Symlink
        } else if ft.is_fifo() {
            FileType::NamedPipe
        } else if ft.is_socket() {
            FileType::Socket
        } else if ft.is_char_device() {
            FileType::CharDevice
        } else if ft.is_block_device() {
            FileType::BlockDevice
        } else {
            FileType::RegularFile
        };

        FileAttr {
            ino,
            size: meta.len(),
            blocks: meta.blocks(),
            atime: meta.accessed().unwrap_or(SystemTime::now()),
            mtime: meta.modified().unwrap_or(SystemTime::now()),
            ctime: if meta.ctime() >= 0 {
                UNIX_EPOCH + Duration::from_secs(meta.ctime() as u64)
            } else {
                UNIX_EPOCH
            },
            crtime: SystemTime::now(),
            kind,
            perm: meta.mode() as u16,
            nlink: meta.nlink() as u32,
            uid: meta.uid(),
            gid: meta.gid(),
            rdev: meta.rdev() as u32,
            blksize: meta.blksize() as u32,
            flags: 0,
        }
    }

    /// Maps a `DirEntry`'s file type to a FUSE `FileType`, covering all Unix types.
    ///
    /// This helper is used during directory listing (`readdir`) to ensure that the
    /// entry type reported to the FUSE client matches the actual type on the host
    /// filesystem (e.g., distinguishing between regular files, directories, and sockets).
    ///
    /// # Arguments
    /// * `entry` - A reference to the standard library's `fs::DirEntry` found during directory iteration.
    ///
    /// # Returns
    /// * The corresponding `fuser::FileType` (Directory, Symlink, NamedPipe, Socket,
    ///   CharDevice, BlockDevice, or RegularFile as a fallback).
    fn entry_file_type(entry: &fs::DirEntry) -> FileType {
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => FileType::Directory,
            Ok(ft) if ft.is_symlink() => FileType::Symlink,
            Ok(ft) if ft.is_fifo() => FileType::NamedPipe,
            Ok(ft) if ft.is_socket() => FileType::Socket,
            Ok(ft) if ft.is_char_device() => FileType::CharDevice,
            Ok(ft) if ft.is_block_device() => FileType::BlockDevice,
            _ => FileType::RegularFile,
        }
    }

    /// Recursively copies a directory tree from source to destination.
    ///
    /// Used during renames of directories that only exist in the lower layer.
    ///
    /// # Arguments
    /// * `src` - Path to the source directory.
    /// * `dst` - Path to the destination directory.
    ///
    /// # Returns
    /// * `Ok(())` on success, or an `std::io::Result` on failure.
    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dst)?;
        let src_meta = fs::symlink_metadata(src)?;
        fs::set_permissions(dst, src_meta.permissions())?;
        Self::copy_ownership_and_times(src, dst, &src_meta);

        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());
            let ft = entry.file_type()?;
            let entry_meta = fs::symlink_metadata(&src_path)?;

            if ft.is_dir() {
                Self::copy_dir_all(&src_path, &dst_path)?;
            } else if ft.is_symlink() {
                let target = fs::read_link(&src_path)?;
                std::os::unix::fs::symlink(&target, &dst_path)?;
                Self::copy_ownership_and_times(&src_path, &dst_path, &entry_meta);
            } else {
                fs::copy(&src_path, &dst_path)?;
                fs::set_permissions(&dst_path, entry_meta.permissions())?;
                Self::copy_ownership_and_times(&src_path, &dst_path, &entry_meta);
            }
        }
        Ok(())
    }

    /// Copies uid, gid, and atime/mtime from `src` metadata onto `dst` without following symlinks.
    /// Applies uid, gid, atime, and mtime from `meta` onto `dst` without following symlinks.
    ///
    /// Use `lchown(2)` and `utimensat(2)` with `AT_SYMLINK_NOFOLLOW` so that symlinks
    /// themselves are updated rather than their targets.
    ///
    /// # Arguments
    /// * `src` - Unused; kept for call-site symmetry with `fs::copy`. The caller has
    ///   already got the metadata via `fs::symlink_metadata(src)`.
    /// * `dst` - The destination path whose ownership and timestamps will be updated.
    /// * `meta` - Metadata from the source entry, providing uid, gid, atime, and mtime.
    fn copy_ownership_and_times(src: &Path, dst: &Path, meta: &fs::Metadata) {
        let _ = src;
        let Ok(dst_c) = CString::new(dst.as_os_str().as_bytes()) else {
            return;
        };
        unsafe {
            libc::lchown(dst_c.as_ptr(), meta.uid(), meta.gid());
            let times = [
                libc::timespec {
                    tv_sec: meta.atime() as libc::time_t,
                    tv_nsec: meta.atime_nsec() as libc::c_long,
                },
                libc::timespec {
                    tv_sec: meta.mtime() as libc::time_t,
                    tv_nsec: meta.mtime_nsec() as libc::c_long,
                },
            ];
            libc::utimensat(
                libc::AT_FDCWD,
                dst_c.as_ptr(),
                times.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            );
        }
    }

    /// Opens `path` with `O_WRONLY` and returns the raw fd.
    ///
    /// This is a low-level helper that wraps the `libc::open` system call to get
    /// a write-only file descriptor. It is particularly useful for operations
    /// like `fallocate` or `copy_file_range` that require raw descriptors.
    ///
    /// # Safety
    /// Caller is responsible for closing the fd with `libc::close` to prevent resource leaks.
    ///
    /// # Arguments
    /// * `path` - The filesystem path to the file to be opened.
    ///
    /// # Returns
    /// * `Ok(libc::c_int)` containing the raw file descriptor on success.
    /// * `Err` if the path contains null bytes or the system call fails.
    fn open_wronly_fd(path: &Path) -> std::io::Result<libc::c_int> {
        let path_c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_WRONLY) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }

    /// Opens `path` with `O_RDONLY` and returns the raw fd.
    ///
    /// This helper wraps the `libc::open` system call to provide a read-only
    /// file descriptor. It is used when a raw descriptor is needed for system
    /// calls that do not require write access, such as the source in `copy_file_range`.
    ///
    /// # Safety
    /// Caller is responsible for closing the fd with `libc::close` to avoid leaking
    /// file descriptors.
    ///
    /// # Arguments
    /// * `path` - The filesystem path to the file to be opened.
    ///
    /// # Returns
    /// * `Ok(libc::c_int)` containing the raw file descriptor on success.
    /// * `Err` if the path conversion fails or the `open` call returns an error.
    fn open_rdonly_fd(path: &Path) -> std::io::Result<libc::c_int> {
        let path_c = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(fd)
        }
    }

    /// Checks whether an upper-layer directory is marked as opaque.
    ///
    /// An opaque directory completely replaces its lower-layer counterpart: none of
    /// the lower entries should be visible through the merge. The Linux kernel overlayfs
    /// sets `trusted.overlay.opaque = y` on such directories; unprivileged tools like
    /// bwrap and proot use the `user.overlay.opaque` namespace instead.
    ///
    /// # Arguments
    /// * `upper_dir` - Absolute path to the directory in the upper layer to inspect.
    ///
    /// # Returns
    /// * `true` if either xattr is present and set to `"y"`, `false` otherwise
    ///   (including when the path contains null bytes or the xattr syscall fails).
    fn is_opaque_dir(upper_dir: &std::path::PathBuf) -> bool {
        let Ok(path_c) = CString::new(upper_dir.as_os_str().as_bytes()) else {
            return false;
        };
        for name in [
            b"trusted.overlay.opaque\0" as &[u8],
            b"user.overlay.opaque\0",
        ] {
            let name_ptr = name.as_ptr() as *const libc::c_char;
            let mut val = [0u8; 2];
            let len = unsafe {
                libc::lgetxattr(
                    path_c.as_ptr(),
                    name_ptr,
                    val.as_mut_ptr() as *mut libc::c_void,
                    val.len(),
                )
            };
            if len == 1 && val[0] == b'y' {
                return true;
            }
        }
        false
    }
}

impl Filesystem for OverlayOps {
    /// Looks up a directory entry by name and returns its attributes.
    ///
    /// This method resolves whether a file exists in the upper or lower layer,
    /// while checking for whiteouts that might hide lower-layer entries.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context (PID, UID, GID of the caller).
    /// * `parent` - The Inode number of the parent directory.
    /// * `name` - The filename to look up within the parent directory.
    /// * `reply` - The callback to send the lookup result (entry attributes) or an error.
    ///
    /// # Returns
    /// * Calls `reply.entry` with metadata if found.
    /// * Call `reply.error` with `ENOENT` if the file is hidden or non-existent.
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let rel = self.inodes.child_path(parent, name);
        if self.layers.is_hidden(&rel) {
            return reply.error(Errno::from_i32(libc::ENOENT));
        }

        let Some((full, _)) = self.layers.resolve(&rel) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        match fs::symlink_metadata(&full) {
            Ok(meta) => {
                let ino = self.inodes.get_ino(&rel);
                reply.entry(&TTL, &self.make_attr(ino, &meta), Generation(0));
            }
            Err(_) => reply.error(Errno::from_i32(libc::ENOENT)),
        }
    }

    /// Notifies the filesystem that the kernel no longer tracks an inode.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The raw inode number being forgotten.
    /// * `_nlookup` - The number of lookups the kernel is forgetting.
    ///
    /// # Returns
    /// * This function does not return a value, as the FUSE protocol does not
    ///   expect a reply for the `forget` operation.
    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.inodes.forget_ino(ino, nlookup);
    }

    /// Gets file attributes (stat).
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the target file/directory.
    /// * `_fh` - An optional file handle if the file is already open.
    /// * `reply` - The callback to send the `FileAttr` structure.
    ///
    /// # Returns
    /// * Calls `reply.attr` with the resolved metadata.
    /// * Call `reply.error` if the inode or path is invalid.
    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        if self.layers.is_hidden(&path) {
            return reply.error(Errno::from_i32(libc::ENOENT));
        }
        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        match fs::symlink_metadata(&full) {
            Ok(meta) => reply.attr(&TTL, &self.make_attr(ino, &meta)),
            Err(_) => reply.error(Errno::from_i32(libc::ENOENT)),
        }
    }

    /// Sets file attributes (chmod, chown, truncate, utimes).
    ///
    /// This operation triggers a Copy-on-Write (CoW) if the file resides in the lower layer,
    /// as metadata changes must be persisted in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - Inode number of the target.
    /// * `mode` - Optional new file permissions.
    /// * `uid` - Optional new user ID.
    /// * `gid` - Optional new group ID.
    /// * `size` - Optional new size (for truncation).
    /// * `atime` - Optional new access time.
    /// * `mtime` - Optional new modification time.
    /// * `ctime` - Optional new change time.
    /// * `_fh` - Optional file handle.
    /// * `_crtime` - Optional creation time (macOS).
    /// * `_chgtime` - Optional attribute change time.
    /// * `_bkuptime` - Optional backup time.
    /// * `_flags` - Optional BSD file flags.
    /// * `reply` - Callback to return the updated attributes.
    ///
    /// # Returns
    /// * Calls `reply.attr` after applying changes in the upper layer.
    /// * Call `reply.error` on permission or I/O failures.
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let upper = match self.layers.copy_on_write(&path) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };

        let is_symlink = fs::symlink_metadata(&upper)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false);

        if let Some(new_size) = size {
            if is_symlink {
                return reply.error(Errno::from_i32(libc::EINVAL));
            }
            if let Err(e) = OpenOptions::new()
                .write(true)
                .open(&upper)
                .and_then(|f| f.set_len(new_size))
            {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        if let Some(m) = mode {
            if !is_symlink {
                if let Err(e) = fs::set_permissions(&upper, fs::Permissions::from_mode(m)) {
                    return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                }
            }
        }

        if uid.is_some() || gid.is_some() {
            let Ok(path_c) = CString::new(upper.as_os_str().as_bytes()) else {
                return reply.error(Errno::from_i32(libc::EINVAL));
            };
            let u = uid.map(|v| v as libc::uid_t).unwrap_or(u32::MAX);
            let g = gid.map(|v| v as libc::gid_t).unwrap_or(u32::MAX);
            if unsafe { libc::lchown(path_c.as_ptr(), u, g) } != 0 {
                return reply.error(Errno::from_i32(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                ));
            }
        }

        if atime.is_some() || mtime.is_some() {
            let Ok(path_c) = CString::new(upper.as_os_str().as_bytes()) else {
                return reply.error(Errno::from_i32(libc::EINVAL));
            };
            let to_ts = |t: Option<TimeOrNow>| -> libc::timespec {
                match t {
                    Some(TimeOrNow::SpecificTime(st)) => {
                        let d = st
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default();
                        libc::timespec {
                            tv_sec: d.as_secs() as libc::time_t,
                            tv_nsec: d.subsec_nanos() as libc::c_long,
                        }
                    }
                    _ => libc::timespec {
                        tv_sec: 0,
                        tv_nsec: libc::UTIME_NOW,
                    },
                }
            };
            let times = [to_ts(atime), to_ts(mtime)];
            if unsafe {
                libc::utimensat(
                    libc::AT_FDCWD,
                    path_c.as_ptr(),
                    times.as_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } != 0
            {
                return reply.error(Errno::from_i32(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                ));
            }
        }

        self.getattr(_req, ino, None, reply);
    }

    /// Reads the target of a symbolic link.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - Inode number of the symlink.
    /// * `reply` - Callback to return the link destination as a byte array.
    ///
    /// # Returns
    /// * `reply.data` with the path string.
    /// * `reply.error` if the inode does not point to a symlink.
    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        match fs::read_link(&full) {
            Ok(target) => reply.data(target.as_os_str().as_encoded_bytes()),
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Creates a file node (regular file, device special file, or pipe).
    ///
    /// This method ensures the node is created in the upper layer. If a whiteout exists
    /// for this name (masking a lower file), it is removed to make the new node visible.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context containing caller's UID, GID, and PID.
    /// * `parent` - The Inode number of the parent directory where the node will be created.
    /// * `name` - The name of the new node to be created.
    /// * `mode` - The file type and permissions for the new node.
    /// * `umask` - The process umask to be applied to the mode.
    /// * `rdev` - The device ID (major/minor) if creating a character or block device.
    /// * `reply` - The callback to return the new entry's attributes or an error.
    ///
    /// # Returns
    /// * Calls `reply.entry` with the new file's metadata on success.
    /// * Call `reply.error` with the corresponding `libc` error code on failure.
    fn mknod(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let rel = self.inodes.child_path(parent, name);
        let upper_path = self.layers.backend.upper.join(&rel);

        if let Some(p) = upper_path.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        let Ok(path_c) = CString::new(upper_path.as_os_str().as_bytes()) else {
            return reply.error(Errno::from_i32(libc::EINVAL));
        };

        let ret = unsafe {
            libc::mknod(
                path_c.as_ptr(),
                (mode & !umask) as libc::mode_t,
                rdev as libc::dev_t,
            )
        };
        if ret != 0 {
            return reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        }

        self.layers.clear_whiteout(&rel);
        match fs::symlink_metadata(&upper_path) {
            Ok(meta) => {
                let ino = self.inodes.get_ino(&rel);
                reply.entry(&TTL, &self.make_attr(ino, &meta), Generation(0));
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Creates a new directory in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - Inode number of the parent directory.
    /// * `name` - The name of the new directory.
    /// * `mode` - The permissions for the new directory.
    /// * `umask` - The process umask to apply to the permissions.
    /// * `reply` - The callback to return the new directory's attributes.
    ///
    /// # Returns
    /// * Calls `reply.entry` with the directory metadata if created successfully.
    /// * Call `reply.error` with `EEXIST` if the directory already exists (POSIX).
    /// * Call `reply.error` if directory creation fails or whiteout cannot be cleared.
    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let rel = self.inodes.child_path(parent, name);
        let upper_path = self.layers.backend.upper.join(&rel);

        // Ensure ancestor directories exist in the upper layer.
        if let Some(p) = upper_path.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        // Use create_dir (not create_dir_all) so that trying to mkdir an
        // already-existing directory returns EEXIST, as POSIX requires.
        if let Err(e) = fs::create_dir(&upper_path) {
            return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
        }

        let _ = fs::set_permissions(
            &upper_path,
            fs::Permissions::from_mode(mode & !umask & 0o7777),
        );
        self.layers.clear_whiteout(&rel);

        // If a directory with the same name exists in the lower layer, mark the new
        // upper directory as opaque (trusted.overlay.opaque = y). Without this, a
        // rmdir + mkdir cycle would cause the old lower entries to reappear through
        // the merge, because there is no individual whiteout for each child.
        // Bwrap and proot rely on this behavior when they recreate dirs like /tmp.
        let lower_dir = self.layers.backend.lower.join(&rel);
        if fs::symlink_metadata(&lower_dir)
            .map(|m| m.is_dir())
            .unwrap_or(false)
        {
            if let Ok(path_c) = CString::new(upper_path.as_os_str().as_bytes()) {
                for attr_name in [
                    b"trusted.overlay.opaque\0" as &[u8],
                    b"user.overlay.opaque\0",
                ] {
                    let name_ptr = attr_name.as_ptr() as *const libc::c_char;
                    unsafe {
                        libc::lsetxattr(
                            path_c.as_ptr(),
                            name_ptr,
                            b"y".as_ptr() as *const libc::c_void,
                            1,
                            0,
                        );
                    }
                }
            }
        }

        match fs::metadata(&upper_path) {
            Ok(meta) => {
                let ino = self.inodes.get_ino(&rel);
                reply.entry(&TTL, &self.make_attr(ino, &meta), Generation(0));
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Removes a file or a symbolic link.
    ///
    /// If the file only exists in the lower layer, this function creates a whiteout
    /// in the upper layer to hide it. If it exists in the upper layer, it is deleted.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - Inode number of the parent directory.
    /// * `name` - Name of the file/link to be removed.
    /// * `reply` - The callback to confirm completion or return an error.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the file was successfully masked or deleted.
    /// * Call `reply.error` with `EISDIR` if the path is a directory (POSIX).
    /// * Call `reply.error` on I/O failures.
    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let rel = self.inodes.child_path(parent, name);
        let upper_path = self.layers.backend.upper.join(&rel);

        // POSIX: unlink on a directory must return EISDIR.
        // Check via resolve so we catch the case where only the lower has it.
        if let Some((full, _)) = self.layers.resolve(&rel) {
            if let Ok(meta) = fs::symlink_metadata(&full) {
                if meta.is_dir() {
                    return reply.error(Errno::from_i32(libc::EISDIR));
                }
            }
        }

        if fs::symlink_metadata(&upper_path).is_ok() {
            if let Err(e) = fs::remove_file(&upper_path) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }
        if fs::symlink_metadata(self.layers.backend.lower.join(&rel)).is_ok() {
            if let Err(e) = self.layers.create_whiteout(&rel) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }
        self.inodes.remove_ino(&rel);
        reply.ok()
    }

    /// Removes a directory.
    ///
    /// A directory can only be removed if it is empty (no visible entries after whiteouts).
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - Inode number of the parent directory.
    /// * `name` - Name of the directory to be removed.
    /// * `reply` - The callback to confirm completion or return an error.
    ///
    /// # Returns
    /// * Calls `reply.ok` on successful removal.
    /// * Call `reply.error` with `ENOTEMPTY` if the directory is not empty.
    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let rel = self.inodes.child_path(parent, name);
        let upper_path = self.layers.backend.upper.join(&rel);
        let lower_path = self.layers.backend.lower.join(&rel);

        // Collect names masked by whiteouts in upper.
        let upper_whiteouts: HashSet<String> = fs::read_dir(&upper_path)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().to_string();
                        n.starts_with(WH_PREFIX)
                            .then(|| n.replacen(WH_PREFIX, "", 1))
                    })
                    .collect()
            })
            .unwrap_or_default();

        let upper_has_real = fs::symlink_metadata(&upper_path).is_ok()
            && fs::read_dir(&upper_path)
                .map(|rd| {
                    rd.flatten()
                        .any(|e| !e.file_name().to_string_lossy().starts_with(WH_PREFIX))
                })
                .unwrap_or(false);

        let lower_has_visible = fs::symlink_metadata(&lower_path).is_ok()
            && fs::read_dir(&lower_path)
                .map(|rd| {
                    rd.flatten().any(|e| {
                        !upper_whiteouts.contains(&e.file_name().to_string_lossy().to_string())
                    })
                })
                .unwrap_or(false);

        if upper_has_real || lower_has_visible {
            return reply.error(Errno::from_i32(libc::ENOTEMPTY));
        }

        if fs::symlink_metadata(&upper_path).is_ok() {
            if let Err(e) = fs::remove_dir_all(&upper_path) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }
        if fs::symlink_metadata(&lower_path).is_ok() {
            let _ = self.layers.create_whiteout(&rel);
        }
        self.inodes.remove_subtree(&rel);
        reply.ok()
    }

    /// Creates a symbolic link in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - Inode number of the parent directory.
    /// * `link_name` - The name of the symlink to be created.
    /// * `target` - The path the symlink should point to.
    /// * `reply` - The callback to return the new link's metadata.
    ///
    /// # Returns
    /// * Calls `reply.entry` with metadata on success.
    /// * Calls `reply.error` on system call failures.
    fn symlink(
        &self,
        _req: &Request,
        parent: INodeNo,
        link_name: &OsStr,
        target: &Path,
        reply: ReplyEntry,
    ) {
        let rel = self.inodes.child_path(parent, link_name);
        let upper_path = self.layers.backend.upper.join(&rel);

        if let Some(p) = upper_path.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        match std::os::unix::fs::symlink(target, &upper_path) {
            Ok(_) => {
                self.layers.clear_whiteout(&rel);
                match fs::symlink_metadata(&upper_path) {
                    Ok(meta) => {
                        let ino = self.inodes.get_ino(&rel);
                        reply.entry(&TTL, &self.make_attr(ino, &meta), Generation(0));
                    }
                    Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Renames or moves an entry.
    ///
    /// If the entry is in the lower layer, it performs a Copy-on-Write of the entire
    /// entry (including subdirectories) before moving it within the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - Inode of the current parent directory.
    /// * `name` - Current name of the entry.
    /// * `newparent` - Inode of the target parent directory.
    /// * `newname` - New name of the entry.
    /// * `flags` - Rename flags (e.g., NOREPLACE, EXCHANGE).
    /// * `reply` - The callback to confirm status.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the move was successful.
    /// * Call `reply.error` if the source doesn't exist or target is invalid.
    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let old_rel = self.inodes.child_path(parent, name);
        let new_rel = self.inodes.child_path(newparent, newname);

        let old_lower = self.layers.backend.lower.join(&old_rel);
        let old_upper = self.layers.backend.upper.join(&old_rel);
        let new_upper = self.layers.backend.upper.join(&new_rel);

        if flags.contains(RenameFlags::RENAME_NOREPLACE) {
            if self.layers.resolve(&new_rel).is_some() {
                return reply.error(Errno::from_i32(libc::EEXIST));
            }
        }

        if flags.contains(RenameFlags::RENAME_EXCHANGE) {
            return reply.error(Errno::from_i32(libc::ENOTSUP));
        }

        if fs::symlink_metadata(&old_upper).is_err() {
            if old_lower.is_dir() {
                if let Err(e) = Self::copy_dir_all(&old_lower, &old_upper) {
                    return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                }
            } else if let Err(e) = self.layers.copy_on_write(&old_rel) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        if let Some(p) = new_upper.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        if let Err(e) = fs::rename(&old_upper, &new_upper) {
            return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
        }

        if fs::symlink_metadata(&old_lower).is_ok() {
            let _ = self.layers.create_whiteout(&old_rel);
        }

        self.layers.clear_whiteout(&new_rel);
        self.inodes.remove_subtree(&new_rel);
        self.inodes.remove_subtree(&old_rel);
        reply.ok()
    }

    /// Creates a hard link to an existing file.
    ///
    /// Triggers a Copy-on-Write for the source file if it is currently in the lower layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the existing source file.
    /// * `newparent` - The Inode of the target parent directory.
    /// * `newname` - The name of the new hard link.
    /// * `reply` - The callback to return the new entry attributes.
    ///
    /// # Returns
    /// * Calls `reply.entry` with attributes on success.
    /// * Call `reply.error` on link creation failures.
    fn link(
        &self,
        _req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let Some(src_rel) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let src_upper = match self.layers.copy_on_write(&src_rel) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };

        let dst_rel = self.inodes.child_path(newparent, newname);
        let dst_upper = self.layers.backend.upper.join(&dst_rel);

        if let Some(p) = dst_upper.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        if let Err(e) = fs::hard_link(&src_upper, &dst_upper) {
            return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
        }

        self.layers.clear_whiteout(&dst_rel);
        match fs::symlink_metadata(&dst_upper) {
            Ok(meta) => {
                let new_ino = self.inodes.get_ino(&dst_rel);
                reply.entry(&TTL, &self.make_attr(new_ino, &meta), Generation(0));
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Opens a file for reading or writing.
    ///
    /// File handles (fh) are not tracked (stateless), but `O_TRUNC` is honored
    /// explicitly: if the caller opens for writing with truncation, we perform a
    /// Copy-on-Write to the upper layer and then truncate the file to zero.
    /// Without this, a shell redirect (`>file`) would leave the old content
    /// beyond the newly written range, silently corrupting the file.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `flags` - Access flags from the caller (O_RDONLY / O_WRONLY / O_RDWR / O_TRUNC …).
    /// * `reply` - The callback to return the file handle.
    ///
    /// # Returns
    /// * Calls `reply.opened` with a default file handle (0).
    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let raw: i32 = flags.0;
        let access = raw & libc::O_ACCMODE;
        let wants_write = access == libc::O_WRONLY || access == libc::O_RDWR;
        let wants_trunc = raw & libc::O_TRUNC != 0;

        if wants_write {
            let Some(path) = self.inodes.get_path(ino) else {
                return reply.error(Errno::from_i32(libc::ENOENT));
            };
            match self.layers.copy_on_write(&path) {
                Ok(upper) => {
                    if wants_trunc {
                        if let Err(e) = OpenOptions::new().write(true).truncate(true).open(&upper) {
                            return reply
                                .error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                        }
                    }
                }
                Err(e) => {
                    return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                }
            }
        }

        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    /// Reads data from a file at a specific offset.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `_fh` - The file handle.
    /// * `offset` - The position to start reading from.
    /// * `size` - The number of bytes to read.
    /// * `_flags` - Opening flags.
    /// * `_lock` - Optional lock owner information.
    /// * `reply` - The callback containing the read data.
    ///
    /// # Returns
    /// * Calls `reply.data` with the buffer content.
    /// * Call `reply.error` on read or seek errors.
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        match fs::File::open(&full) {
            Ok(mut file) => {
                let mut buf = vec![0u8; size as usize];
                if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                    return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                }
                let n = file.read(&mut buf).unwrap_or(0);
                reply.data(&buf[..n]);
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Writes data to a file.
    ///
    /// Forces a Copy-on-Write to the upper layer if the file is not already there.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `_fh` - The file handle.
    /// * `offset` - The position to start writing at.
    /// * `data` - The byte buffer containing data to write.
    /// * `_write_flags` - Specific write behavior flags.
    /// * `_flags` - Opening flags.
    /// * `_lock` - Optional lock owner information.
    /// * `reply` - The callback to return the number of bytes written.
    ///
    /// # Returns
    /// * Calls `reply.written` with the byte count on success.
    /// * Call `reply.error` on writing or CoW failures.
    ///
    /// # Note on `truncate(false)`
    /// The `OpenOptions` **must** include `.truncate(false)`.  Without it,
    /// opening an already-existing upper-layer file with `O_WRONLY | O_CREAT`
    /// would silently truncate the file to zero before writing, corrupting any
    /// content outside the range `[offset, offset + data.len())` — a classic
    /// TOCTOU-style data-loss bug.
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let upper = match self.layers.copy_on_write(&path) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };
        match OpenOptions::new().write(true).truncate(false).open(&upper) {
            Ok(mut file) => {
                if let Err(e) = file.seek(SeekFrom::Start(offset)) {
                    return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                }

                match file.write(data) {
                    Ok(n) => reply.written(n as u32),
                    Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Handles the closing of a file descriptor.
    ///
    /// Called on every `close(2)` of a file descriptor. Unlike `fsync`, which the
    /// application calls explicitly, `flush` is a best-effort opportunity to report
    /// any pending writing errors before the close completes.
    ///
    /// For files that were written (i.e., exist in the upper layer) we open them
    /// with write access so that `sync_all` uses a valid writable fd — calling
    /// `fdatasync` on an `O_RDONLY` fd returns `EBADF` on Linux ≥ 5.15.
    /// Files that are only in the lower layer (read-only) need no flushing.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file being flushed.
    /// * `_fh` - The file handle associated with the open file.
    /// * `_lock_owner` - The ID of the lock owner, if any locks are held.
    /// * `reply` - The callback to confirm the flush or return an error.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the file was successfully synced or is not in the upper layer.
    /// * Calls `reply.error(EIO)` if `sync_all` fails on an upper-layer file.
    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.ok();
        };
        let upper = self.layers.backend.upper.join(&path);

        // Only flush if the file actually landed in the upper layer.
        let Ok(meta) = fs::symlink_metadata(&upper) else {
            return reply.ok();
        };
        // Symlinks and directories cannot be meaningfully synced this way.
        if !meta.is_file() {
            return reply.ok();
        }

        // Must open with write access; sync on a read-only fd is EBADF.
        match OpenOptions::new().write(true).open(&upper) {
            Ok(f) => {
                if f.sync_all().is_ok() {
                    reply.ok()
                } else {
                    reply.error(Errno::from_i32(libc::EIO))
                }
            }
            Err(_) => reply.ok(),
        }
    }

    /// Synchronizes dirty file data in memory with storage.
    ///
    /// Only upper-layer files are synced; lower-layer files are read-only and
    /// need no flushing from our side.
    ///
    /// The file must be opened with write access: `fdatasync(2)` and `fsync(2)`
    /// return `EBADF` on a read-only file descriptor on Linux ≥ 5.15.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `_fh` - The file handles.
    /// * `datasync` - If `true`, only synchronizes data (not metadata).
    /// * `reply` - The callback to confirm sync completion.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the sync succeeded or the file is not in the upper layer.
    /// * Call `reply.error` with the errno from `fdatasync`/`fsync` on failure.
    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        datasync: bool,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let upper = self.layers.backend.upper.join(&path);

        // File not in upper → nothing to sync on our side.
        let Ok(meta) = fs::symlink_metadata(&upper) else {
            return reply.ok();
        };
        if !meta.is_file() {
            return reply.ok();
        }

        // Must open with write access; fdatasync/fsync on O_RDONLY is EBADF.
        match OpenOptions::new().write(true).open(&upper) {
            Ok(f) => {
                use std::os::unix::io::AsRawFd;
                let ret = if datasync {
                    unsafe { libc::fdatasync(f.as_raw_fd()) }
                } else {
                    unsafe { libc::fsync(f.as_raw_fd()) }
                };
                if ret == 0 {
                    reply.ok()
                } else {
                    reply.error(Errno::from_i32(
                        std::io::Error::last_os_error()
                            .raw_os_error()
                            .unwrap_or(libc::EIO),
                    ))
                }
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Opens a directory.
    ///
    /// Since this implementation is stateless regarding handles, it returns a
    /// default file handle. Actual entry reading is handled by `readdir`.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `_ino` - The Inode number of the directory to be opened.
    /// * `_flags` - The flags for opening the directory (e.g., O_RDONLY).
    /// * `reply` - The callback to return the directory handle.
    ///
    /// # Returns
    /// * Calls `reply.opened` with a default directory handle (0).
    fn opendir(&self, _req: &Request, _ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    /// Lists directory entries, merging upper and lower layers and handling whiteouts.
    ///
    /// This is a critical operation for OverlayFS: it must show files from both
    /// layers, but hide files in the lower layer that have a corresponding whiteout
    /// in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the directory to list.
    /// * `_fh` - The directory handle (unused in this stateless implementation).
    /// * `offset` - The index in the stream to resume listing from.
    /// * `reply` - The callback to add entries to the directory stream.
    ///
    /// # Returns
    /// * Calls `reply.add` for each valid entry (calculating new offsets).
    /// * Call `reply.ok` once all entries (or the buffer limit) are processed.
    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let rel_path = self.inodes.get_path(ino).unwrap_or_default();
        let parent_ino = if rel_path.as_os_str().is_empty() {
            INodeNo(1)
        } else {
            let parent_path = rel_path.parent().unwrap_or(Path::new(""));
            if parent_path.as_os_str().is_empty() {
                INodeNo(1)
            } else {
                self.inodes.get_ino(parent_path)
            }
        };

        let mut entries: Vec<(INodeNo, FileType, String)> = vec![
            (ino, FileType::Directory, ".".to_string()),
            (parent_ino, FileType::Directory, "..".to_string()),
        ];

        let mut seen: HashSet<String> = HashSet::new();
        let mut whiteouts: HashSet<String> = HashSet::new();

        let upper_dir = self.layers.backend.upper.join(&rel_path);
        let lower_dir = self.layers.backend.lower.join(&rel_path);

        if let Ok(list) = fs::read_dir(&upper_dir) {
            let entries_raw: Vec<_> = list.flatten().collect();
            for entry in &entries_raw {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(WH_PREFIX) {
                    whiteouts.insert(name.replacen(WH_PREFIX, "", 1));
                }
            }
            for entry in entries_raw {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(WH_PREFIX) || whiteouts.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                entries.push((
                    self.inodes.peek_ino(&rel_path.join(&name)),
                    Self::entry_file_type(&entry),
                    name,
                ));
            }
        }

        let upper_is_opaque = Self::is_opaque_dir(&upper_dir);
        if !upper_is_opaque {
            if let Ok(list) = fs::read_dir(&lower_dir) {
                for entry in list.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !seen.contains(&name) && !whiteouts.contains(&name) {
                        entries.push((
                            self.inodes.peek_ino(&rel_path.join(&name)),
                            Self::entry_file_type(&entry),
                            name,
                        ));
                    }
                }
            }
        }

        for (i, (ino, ft, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(ino, (i + 1) as u64, ft, name) {
                break;
            }
        }
        reply.ok()
    }

    /// Lists directory entries with pre-fetched attributes, merging upper and lower layers.
    ///
    /// This is the optimized variant of `readdir`: each entry is returned together with
    /// its full `FileAttr`, allowing the kernel to skip individual `lookup` calls for
    /// every item. The merge logic (whiteout handling, opaque directories) is identical
    /// to `readdir`.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context (UID, GID, PID of the caller).
    /// * `ino` - The Inode number of the directory to list.
    /// * `_fh`    - The directory handle (unused — this implementation is stateless).
    /// * `offset` - Index in the entry stream to resume from (0 = start from beginning).
    /// * `reply`  - The callback used to push entries; returns `true` when the buffer is full.
    ///
    /// # Returns
    /// * Calls `reply.add` for each valid entry until the buffer is full or the list is exhausted.
    /// * Call `reply.ok` once all entries have been processed.
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let rel_path = self.inodes.get_path(ino).unwrap_or_default();
        let parent_ino = if rel_path.as_os_str().is_empty() {
            INodeNo(1)
        } else {
            let parent_path = rel_path.parent().unwrap_or(Path::new(""));
            if parent_path.as_os_str().is_empty() {
                INodeNo(1)
            } else {
                self.inodes.get_ino(parent_path)
            }
        };

        struct PlusEntry {
            ino: INodeNo,
            name: String,
            rel: std::path::PathBuf,
            is_upper: bool,
        }

        let mut entries: Vec<PlusEntry> = Vec::new();

        entries.push(PlusEntry {
            ino,
            name: ".".into(),
            rel: rel_path.clone(),
            is_upper: true,
        });
        let parent_rel = rel_path.parent().unwrap_or(Path::new("")).to_path_buf();
        entries.push(PlusEntry {
            ino: parent_ino,
            name: "..".into(),
            rel: parent_rel,
            is_upper: true,
        });

        let upper_dir = self.layers.backend.upper.join(&rel_path);
        let lower_dir = self.layers.backend.lower.join(&rel_path);
        let mut seen: HashSet<String> = HashSet::new();
        let mut whiteouts: HashSet<String> = HashSet::new();

        if let Ok(list) = fs::read_dir(&upper_dir) {
            let entries_raw: Vec<_> = list.flatten().collect();
            for entry in &entries_raw {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(WH_PREFIX) {
                    whiteouts.insert(name.replacen(WH_PREFIX, "", 1));
                }
            }
            for entry in entries_raw {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(WH_PREFIX) || whiteouts.contains(&name) {
                    continue;
                }
                seen.insert(name.clone());
                entries.push(PlusEntry {
                    ino: self.inodes.peek_ino(&rel_path.join(&name)),
                    name,
                    rel: rel_path.join(entry.file_name()),
                    is_upper: true,
                });
            }
        }
        let upper_is_opaque = Self::is_opaque_dir(&upper_dir);
        if !upper_is_opaque {
            if let Ok(list) = fs::read_dir(&lower_dir) {
                for entry in list.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !seen.contains(&name) && !whiteouts.contains(&name) {
                        entries.push(PlusEntry {
                            ino: self.inodes.peek_ino(&rel_path.join(&name)),
                            name,
                            rel: rel_path.join(entry.file_name()),
                            is_upper: false,
                        });
                    }
                }
            }
        }

        for (i, e) in entries.into_iter().enumerate().skip(offset as usize) {
            let phys = if e.is_upper {
                self.layers.backend.upper.join(&e.rel)
            } else {
                self.layers.backend.lower.join(&e.rel)
            };
            let Ok(meta) = fs::symlink_metadata(&phys) else {
                continue;
            };
            let attr = self.make_attr(e.ino, &meta);
            if reply.add(e.ino, (i + 1) as u64, &e.name, &TTL, &attr, Generation(0)) {
                break;
            }
        }
        reply.ok();
    }

    /// Returns filesystem statistics by querying both layers.
    ///
    /// Total blocks/inodes are taken from the lower layer (read-only content),
    /// while free/available space comes from the upper layer (where writes land).
    /// Block-size fields (`bsize`, `frsize`, `namelen`) are also taken from the
    /// upper layer so that arithmetic such as `bavail * bsize` is self-consistent
    /// for the device that actually accepts new data.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `_ino` - The Inode number (unused).
    /// * `reply` - The callback returning block size, free blocks, etc.
    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        let lower_c = CString::new(self.layers.backend.lower.as_os_str().as_bytes());
        let upper_c = CString::new(self.layers.backend.upper.as_os_str().as_bytes());

        let (Ok(lower_c), Ok(upper_c)) = (lower_c, upper_c) else {
            return reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
        };

        let mut lower_stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let mut upper_stat: libc::statvfs = unsafe { std::mem::zeroed() };

        if unsafe { libc::statvfs(lower_c.as_ptr(), &mut lower_stat) } != 0
            || unsafe { libc::statvfs(upper_c.as_ptr(), &mut upper_stat) } != 0
        {
            return reply.statfs(0, 0, 0, 0, 0, 512, 255, 0);
        }

        reply.statfs(
            lower_stat.f_blocks,
            upper_stat.f_bfree,
            upper_stat.f_bavail,
            lower_stat.f_files,
            upper_stat.f_ffree,
            upper_stat.f_bsize as u32,
            upper_stat.f_namemax as u32,
            upper_stat.f_frsize as u32,
        );
    }

    /// Sets an extended attribute (xattr) on a file.
    ///
    /// Promotes the target to the upper layer before applying the attribute.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `name` - The name of the extended attribute.
    /// * `value` - The byte buffer containing the attribute data.
    /// * `_flags` - Setxattr flags (e.g., CREATE, REPLACE).
    /// * `_position` - Attribute offset (macOS only).
    /// * `reply` - The callback to confirm completion.
    ///
    /// # Returns
    /// * Calls `reply.ok` on success.
    /// * Call `reply.error` on CoW or libc errors.
    fn setxattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let upper = match self.layers.copy_on_write(&path) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };
        let (Ok(path_c), Ok(name_c)) = (
            CString::new(upper.as_os_str().as_bytes()),
            CString::new(name.as_bytes()),
        ) else {
            return reply.error(Errno::from_i32(libc::EINVAL));
        };

        let ret = unsafe {
            libc::lsetxattr(
                path_c.as_ptr(),
                name_c.as_ptr(),
                value.as_ptr() as *const libc::c_void,
                value.len(),
                0,
            )
        };
        if ret != 0 {
            reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        } else {
            reply.ok();
        }
    }

    /// Gets an extended attribute value.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the file.
    /// * `name` - The name of the attribute to fetch.
    /// * `size` - Size of the destination buffer.
    /// * `reply` - The callback returning the data or required size.
    ///
    /// # Returns
    /// * Calls `reply.data` or `reply.size` on success.
    fn getxattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let (Ok(path_c), Ok(name_c)) = (
            CString::new(full.as_os_str().as_bytes()),
            CString::new(name.as_bytes()),
        ) else {
            return reply.error(Errno::from_i32(libc::EINVAL));
        };

        if size == 0 {
            let len = unsafe {
                libc::lgetxattr(path_c.as_ptr(), name_c.as_ptr(), std::ptr::null_mut(), 0)
            };
            if len < 0 {
                reply.error(Errno::from_i32(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                ));
            } else {
                reply.size(len as u32);
            }
        } else {
            let mut buf = vec![0u8; size as usize];
            let len = unsafe {
                libc::lgetxattr(
                    path_c.as_ptr(),
                    name_c.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    size as libc::size_t,
                )
            };
            if len < 0 {
                reply.error(Errno::from_i32(
                    std::io::Error::last_os_error()
                        .raw_os_error()
                        .unwrap_or(libc::EIO),
                ));
            } else {
                reply.data(&buf[..len as usize]);
            }
        }
    }

    /// Lists all extended attribute names for a file or directory.
    ///
    /// Merges xattr names from both the upper and lower layers, deduplicating
    /// names that appear in both. Formats the result as a null-separated byte
    /// sequence as expected by the FUSE kernel driver. When `size == 0`,
    /// returns only the required buffer length without writing any data.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context (UID, GID, PID of the caller).
    /// * `ino` - The Inode number of the target file or directory.
    /// * `size` - The size of the buffer provided by the caller. If 0, the function
    ///            must return the required buffer size to hold all names.
    /// * `reply` - The callback used to send the attribute list or the required size.
    ///
    /// # Returns
    /// * Calls `reply.size` if the input `size` was 0, providing the total byte count.
    /// * Calls `reply.data` with the null-separated list of attribute names.
    /// * Call `reply.error` with `ERANGE` if the provided buffer is too small,
    ///   or `ENOENT` if the path cannot be resolved.
    fn listxattr(&self, _req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        /// Internal helper: Retrieves a raw null-separated list of xattr names from
        /// the physical filesystem using the `llistxattr` system call.
        ///
        /// # Arguments
        /// * `p` - The physical path to the file in a specific layer.
        fn raw_list(p: &Path) -> Vec<u8> {
            let Ok(path_c) = CString::new(p.as_os_str().as_bytes()) else {
                return Vec::new();
            };
            let len = unsafe { libc::llistxattr(path_c.as_ptr(), std::ptr::null_mut(), 0) };
            if len <= 0 {
                return Vec::new();
            }
            let mut buf = vec![0u8; len as usize];
            let n = unsafe {
                libc::llistxattr(
                    path_c.as_ptr(),
                    buf.as_mut_ptr() as *mut libc::c_char,
                    len as libc::size_t,
                )
            };
            if n >= 0 {
                buf.truncate(n as usize);
                buf
            } else {
                Vec::new()
            }
        }

        /// Internal helper: Parses the raw byte buffer into a vector of strings.
        ///
        /// # Arguments
        /// * `raw` - The null-separated byte slice from the kernel.
        fn parse_names(raw: &[u8]) -> Vec<String> {
            raw.split(|&b| b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        }

        let Some((full, is_upper)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let mut names: Vec<String> = parse_names(&raw_list(&full));

        if !is_upper {
            let upper_path = self.layers.backend.upper.join(&path);
            if fs::symlink_metadata(&upper_path).is_ok() {
                for n in parse_names(&raw_list(&upper_path)) {
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
            }
        }

        let mut out: Vec<u8> = Vec::new();
        for n in &names {
            out.extend_from_slice(n.as_bytes());
            out.push(0);
        }

        if size == 0 {
            reply.size(out.len() as u32);
        } else if (size as usize) < out.len() {
            reply.error(Errno::from_i32(libc::ERANGE));
        } else {
            reply.data(&out);
        }
    }

    /// Removes an extended attribute from a file or directory.
    ///
    /// To maintain the integrity of the lower layer, this method triggers a
    /// Copy-on-Write (CoW). The attribute is then removed from the newly
    /// created file in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the target.
    /// * `name` - The name of the extended attribute to be removed.
    /// * `reply` - The callback to confirm the successful removal.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the attribute was successfully removed from the upper layer.
    /// * Calls `reply.error` with `ENOENT` if the file does not exist, or
    ///   the corresponding `libc` error if the removal fails.
    fn removexattr(&self, _req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let upper = match self.layers.copy_on_write(&path) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };
        let (Ok(path_c), Ok(name_c)) = (
            CString::new(upper.as_os_str().as_bytes()),
            CString::new(name.as_bytes()),
        ) else {
            return reply.error(Errno::from_i32(libc::EINVAL));
        };

        let ret = unsafe { libc::lremovexattr(path_c.as_ptr(), name_c.as_ptr()) };
        if ret != 0 {
            reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        } else {
            reply.ok();
        }
    }

    /// Checks if a file exists and if the caller has the requested permissions.
    ///
    /// # Implementation note
    /// `DefaultPermissions` is enabled at mount time, which means the FUSE kernel
    /// driver already performs standard DAC checks **before** dispatching `access`
    /// to us.  Our role here is therefore limited to:
    ///
    /// 1. Confirming the path actually exists (not hidden by a whiteout).
    /// 2. Handling `F_OK` (existence-only checks).
    /// 3. Applying the correct POSIX semantics for `uid 0` (root bypasses all
    ///    permission bits except the executed bit on regular files when `X_OK`
    ///    is requested — and even then only if *no* executed bit is set at all).
    ///
    /// For non-root callers, the kernel has already validated `R_OK`/`W_OK`/`X_OK`
    /// via `DefaultPermissions`, so we return `ok()` for them unconditionally
    /// (the kernel would not have forwarded the call if access were denied).
    ///
    /// # Arguments
    /// * `req` - The FUSE request context (UID, GID, PID of the caller).
    /// * `ino` - The Inode number of the file to check.
    /// * `mask` - The bitmask of permissions to check (R_OK, W_OK, X_OK, F_OK=0).
    /// * `reply` - The callback to return the result.
    ///
    /// # Returns
    /// * Calls `reply.ok` if access is permitted (or if the caller is not root and
    ///   `DefaultPermissions` handles the check at the kernel level).
    /// * Call `reply.error(ENOENT)` if the inode is hidden or unresolvable.
    /// * Calls `reply.error(EACCES)` if root requests execute access on a file with
    ///   no executed bit set for any principal (owner, group, or other).
    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        if self.layers.is_hidden(&path) {
            return reply.error(Errno::from_i32(libc::ENOENT));
        }

        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        if mask.bits() == 0 {
            return reply.ok();
        }

        if req.uid() != 0 {
            return reply.ok();
        }

        if mask.bits() & libc::X_OK != 0 {
            let meta = match fs::symlink_metadata(&full) {
                Ok(m) => m,
                Err(_) => return reply.error(Errno::from_i32(libc::ENOENT)),
            };
            if !meta.file_type().is_dir() && (meta.mode() & 0o111) == 0 {
                return reply.error(Errno::from_i32(libc::EACCES));
            }
        }

        reply.ok()
    }

    /// Creates and opens a new regular file atomically.
    ///
    /// The file is always created in the upper layer, ensuring visibility over
    /// any potential lower-layer content.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `parent` - The Inode number of the parent directory.
    /// * `name` - The name of the new file.
    /// * `mode` - The file permissions.
    /// * `umask` - The process umask.
    /// * `flags` - Opening flags for the new file.
    /// * `reply` - The callback returning the file metadata and handle.
    ///
    /// # Returns
    /// * Calls `reply.created` on success.
    /// * Call `reply.error` on creation failures.
    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let rel = self.inodes.child_path(parent, name);
        let upper_path = self.layers.backend.upper.join(&rel);

        if let Some(p) = upper_path.parent() {
            if let Err(e) = fs::create_dir_all(p) {
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        }

        match fs::File::create(&upper_path) {
            Ok(_) => {
                let _ = fs::set_permissions(
                    &upper_path,
                    fs::Permissions::from_mode(mode & !umask & 0o7777),
                );
                self.layers.clear_whiteout(&rel);
                match fs::metadata(&upper_path) {
                    Ok(meta) => {
                        let ino = self.inodes.get_ino(&rel);
                        reply.created(
                            &TTL,
                            &self.make_attr(ino, &meta),
                            Generation(0),
                            FileHandle(0),
                            FopenFlags::empty(),
                        );
                    }
                    Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
                }
            }
            Err(e) => reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        }
    }

    /// Manipulates the allocated disk space for a file.
    ///
    /// This method allows pre-allocating or deallocating space for a file (e.g., hole punching).
    /// Since it modifies the file's allocation, a Copy-on-Write (CoW) operation is
    /// triggered to ensure the change happens in the upper layer.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context (containing caller's UID, GID, and PID).
    /// * `ino` - The Inode number of the file to modify.
    /// * `_fh` - The file handle (unused in this stateless implementation).
    /// * `offset` - The starting byte offset for the allocation change.
    /// * `length` - The number of bytes to allocate or deallocate.
    /// * `mode` - The specific operation to perform (e.g., `FALLOC_FL_PUNCH_HOLE`).
    /// * `reply` - The callback to confirm the operation's success or return an error.
    ///
    /// # Returns
    /// * Calls `reply.ok` if the allocation was successful in the upper layer.
    /// * Call `reply.error` with the corresponding `libc` error code on failure.
    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let upper = match self.layers.copy_on_write(&path) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };

        let fd = match Self::open_wronly_fd(&upper) {
            Ok(fd) => fd,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };

        let ret =
            unsafe { libc::fallocate(fd, mode, offset as libc::off_t, length as libc::off_t) };
        unsafe { libc::close(fd) };

        if ret != 0 {
            reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        } else {
            reply.ok();
        }
    }

    /// Computes a new file offset without performing any I/O.
    ///
    /// Opens the resolved file read-only to get a real file descriptor, then delegates
    /// to `libc::lseek` so that the host kernel handles sparse-file
    /// semantics ('SEEK_DATA' / 'SEEK_HOLE') correctly. The fd is closed immediately after.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino` - The Inode number of the target file.
    /// * `_fh` - The file handle (unused — this implementation is stateless).
    /// * `offset` - The byte offset to seek from (interpretation depends on `whence`).
    /// * `whence` - Seek mode: `SEEK_SET`, `SEEK_CUR`, `SEEK_END`, `SEEK_DATA`, or `SEEK_HOLE`.
    /// * `reply`  - The callback returning the resulting absolute offset.
    ///
    /// # Returns
    /// * Calls `reply.offset` with the new position on success.
    /// * Call `reply.error` with the errno from `lseek(2)` on failure (e.g. `ENXIO` for
    ///   `SEEK_DATA` past end-of-file, `EINVAL` for an unsupported `whence`).
    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        let Some(path) = self.inodes.get_path(ino) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };
        let Some((full, _)) = self.layers.resolve(&path) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        match whence {
            libc::SEEK_SET => {
                if offset < 0 {
                    return reply.error(Errno::from_i32(libc::EINVAL));
                }
                return reply.offset(offset);
            }
            libc::SEEK_END => {
                let size = match fs::symlink_metadata(&full) {
                    Ok(m) => m.len() as i64,
                    Err(e) => {
                        return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
                    }
                };
                let result = size.checked_add(offset).unwrap_or(-1);
                if result < 0 {
                    return reply.error(Errno::from_i32(libc::EINVAL));
                }
                return reply.offset(result);
            }
            libc::SEEK_CUR => {
                return reply.error(Errno::from_i32(libc::EINVAL));
            }
            _ => {}
        }

        let Ok(path_c) = CString::new(full.as_os_str().as_bytes()) else {
            return reply.error(Errno::from_i32(libc::EINVAL));
        };

        let fd = unsafe { libc::open(path_c.as_ptr(), libc::O_RDONLY) };
        if fd < 0 {
            return reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        }

        let result = unsafe { libc::lseek(fd, offset as libc::off_t, whence) };
        unsafe { libc::close(fd) };

        if result < 0 {
            reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        } else {
            reply.offset(result);
        }
    }

    /// Offloads data copying between two files to the kernel.
    ///
    /// In an OverlayFS, if the destination file is in the lower layer, it must
    /// be promoted to the upper layer via Copy-on-Write before the copy starts.
    ///
    /// # Arguments
    /// * `_req` - The FUSE request context.
    /// * `ino_in` - The Inode number of the source file.
    /// * `_fh_in` - The file handle of the source file.
    /// * `offset_in` - The starting offset in the source file.
    /// * `ino_out` - The Inode number of the destination file.
    /// * `_fh_out` - The file handle of the destination file.
    /// * `offset_out` - The starting offset in the destination file.
    /// * `len` - The total number of bytes to copy.
    /// * `_flags` - Copy flags (unused).
    /// * `reply` - The callback returning the actual number of bytes copied.
    ///
    /// # Returns
    /// * Calls `reply.written` with the number of bytes copied on success.
    /// * Call `reply.error` on I/O or CoW failures.
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        _fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        _fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        _flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        let (Some(path_in), Some(path_out)) =
            (self.inodes.get_path(ino_in), self.inodes.get_path(ino_out))
        else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let Some((src_full, _)) = self.layers.resolve(&path_in) else {
            return reply.error(Errno::from_i32(libc::ENOENT));
        };

        let dst_upper = match self.layers.copy_on_write(&path_out) {
            Ok(p) => p,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };

        let fd_in = match Self::open_rdonly_fd(&src_full) {
            Ok(fd) => fd,
            Err(e) => return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO))),
        };
        let fd_out = match Self::open_wronly_fd(&dst_upper) {
            Ok(fd) => fd,
            Err(e) => {
                unsafe { libc::close(fd_in) };
                return reply.error(Errno::from_i32(e.raw_os_error().unwrap_or(libc::EIO)));
            }
        };

        let mut off_in = offset_in as libc::off64_t;
        let mut off_out = offset_out as libc::off64_t;
        let copied = unsafe {
            libc::copy_file_range(
                fd_in,
                &mut off_in,
                fd_out,
                &mut off_out,
                len as libc::size_t,
                0,
            )
        };
        unsafe {
            libc::close(fd_in);
            libc::close(fd_out);
        }

        if copied < 0 {
            reply.error(Errno::from_i32(
                std::io::Error::last_os_error()
                    .raw_os_error()
                    .unwrap_or(libc::EIO),
            ));
        } else {
            reply.written(copied as u32);
        }
    }
}
