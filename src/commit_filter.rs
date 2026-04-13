//! Commit filtering for rootfs-aware OverlayFS merges.
//!
//! When committing an overlay that wraps a rootfs (used by `bwrap`, `proot`,
//! or similar sandboxing tools), many paths must be excluded from the merge:
//!
//! - **Virtual/kernel-managed directories** (`/proc`, `/sys`, `/dev`) – their
//!   contents are populated by the kernel at runtime and must never be written
//!   to the lower layer.
//! - **Bind-mount targets** (`/tmp`, `/mnt`, `/home`, `/run`, `/media`) – these
//!   are typically replaced wholesale by the sandbox and their upper-layer
//!   shadows carry no meaningful state.
//! - **Zero-permission files** (mode `0o000`) – these are almost always whiteout
//!   artifacts, device stubs, or deliberately inaccessible entries that should
//!   not propagate.
//! - **Custom paths / filenames** – caller-supplied lists for project-specific
//!   exclusions.
//!
//! # Usage
//!
//! ```rust,no_run
//! use overlay_fuse::CommitFilter;
//!
//! let filter = CommitFilter::rootfs()          // sensible defaults for a rootfs overlay
//!     .skip_dir("/opt/scratch")                // ignore an extra directory
//!     .skip_file("lost+found")                 // ignore a specific filename anywhere
//!     .skip_zero_permissions(true);            // already on for rootfs(), shown for clarity
//!
//! overlay.set_commit_filter(filter);
//! ```

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::fs;

/// Controls which paths are excluded when committing upper-layer changes to lower.
///
/// A path is skipped when **any** of the following conditions match:
///
/// 1. Its exact filename appears in [`skip_files`] (applied at every depth).
/// 2. Its root-relative path matches, or is a descendant of, an entry in
///    [`skip_dirs`].
/// 3. Its Unix permission bits are `0o000` and [`skip_zero_permissions`] is
///    enabled (checked for non-symlink entries only, since symlinks always
///    report `0o777` on Linux).
///
/// All checks are performed on the **relative** path inside the upper layer
/// (i.e., the path as it would appear inside the mounted rootfs), so the caller
/// does not need to know the physical location of the upper directory.
///
/// [`skip_files`]: CommitFilter::skip_files
/// [`skip_dirs`]: CommitFilter::skip_dirs
/// [`skip_zero_permissions`]: CommitFilter::skip_zero_permissions
#[derive(Debug, Clone)]
pub struct CommitFilter {
    /// Root-relative directory paths to skip entirely (e.g. `"dev"`, `"proc"`).
    ///
    /// Each entry is matched against the leading components of the relative
    /// path being committed.  A directory **and all its descendants** are
    /// excluded when the relative path starts with one of these prefixes.
    /// Comparisons are component-level (via `Path::strip_prefix`), so `"dev"`
    /// never accidentally matches `"devices"`.
    skip_dirs: HashSet<PathBuf>,

    /// Exact file *names* (not full paths) that should never be committed,
    /// regardless of the directory they live in.
    ///
    /// Useful for sentinel files like `lost+found`, `.gitkeep`, or overlay
    /// opaque-whiteout markers (`.wh..wh..opq`) that should not leak into lower.
    skip_files: HashSet<String>,

    /// When `true`, any entry whose Unix permission bits are exactly `0o000`
    /// is excluded from the commit.
    ///
    /// In rootfs overlays these entries are typically kernel-managed device
    /// stubs, intentionally inaccessible sockets, or artifacts left by the
    /// sandbox runtime that carry no meaning in the lower layer.
    skip_zero_permissions: bool,
}

impl Default for CommitFilter {
    /// Returns an empty filter that allows every entry through unchanged.
    fn default() -> Self {
        Self {
            skip_dirs: HashSet::new(),
            skip_files: HashSet::new(),
            skip_zero_permissions: false,
        }
    }
}

impl CommitFilter {
    /// Creates an empty filter – nothing is skipped.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a filter pre-populated with the directories and rules that are
    /// appropriate for a rootfs-based overlay (the kind managed by `bwrap` or
    /// `proot`).
    ///
    /// The following root-level directories are excluded:
    ///
    /// | Path      | Reason |
    /// |-----------|--------|
    /// | `/dev`    | Character/block devices managed by the kernel; never real files. |
    /// | `/proc`   | Virtual procfs; kernel-generated, mounts change per-process. |
    /// | `/sys`    | sysfs; kernel ABI, always bind-mounted from the host. |
    /// | `/run`    | Runtime state (PID files, sockets); meaningless after session ends. |
    /// | `/tmp`    | Temporary files; bwrap/proot typically bind-mount a fresh tmpfs here. |
    /// | `/mnt`    | Generic mount target; typically used as a bind entry point. |
    /// | `/media`  | Removable-media mount points; host-managed. |
    /// | `/home`   | User home directories; bwrap bind-mounts the real home here. |
    ///
    /// Zero-permission skipping is also enabled because rootfs overlays
    /// routinely produce `0o000` stubs for `null`, `zero`, `random`, etc.
    pub fn rootfs() -> Self {
        const ROOTFS_SKIP_DIRS: &[&str] = &[
            "dev", "proc", "sys", "run", "tmp", "mnt", "media", "home",
        ];

        let mut filter = Self::new();
        filter.skip_zero_permissions = true;

        for dir in ROOTFS_SKIP_DIRS {
            filter.skip_dirs.insert(PathBuf::from(dir));
        }

        filter
    }

    // ── Builder methods ──────────────────────────────────────────────────────

    /// Adds a root-relative directory path that should be excluded from the
    /// commit, including all of its descendants.
    ///
    /// A leading `/` is stripped so that `"/dev"` and `"dev"` are treated
    /// identically.
    ///
    /// # Arguments
    /// * `path` – Any value convertible to a `Path` (e.g. `&str`, `String`,
    ///   `PathBuf`).
    ///
    /// # Returns
    /// * `Self` with the new exclusion added (builder pattern).
    pub fn skip_dir(mut self, path: impl AsRef<Path>) -> Self {
        let p = path.as_ref();
        let stripped = p.strip_prefix("/").unwrap_or(p);
        self.skip_dirs.insert(stripped.to_path_buf());
        self
    }

    /// Adds an exact filename that should never be committed, at any depth.
    ///
    /// The match is against the bare filename component only; the containing
    /// directory is not considered.
    ///
    /// # Arguments
    /// * `name` – The bare filename (e.g. `"lost+found"`, `".gitkeep"`).
    ///
    /// # Returns
    /// * `Self` with the filename exclusion added (builder pattern).
    pub fn skip_file(mut self, name: impl Into<String>) -> Self {
        self.skip_files.insert(name.into());
        self
    }

    /// Controls whether entries with Unix permissions `0o000` are excluded.
    ///
    /// Symlinks are exempt from this check because Linux always reports their
    /// mode as `0o777`.
    ///
    /// # Arguments
    /// * `enabled` – `true` to skip zero-permission entries.
    ///
    /// # Returns
    /// * `Self` (builder pattern).
    pub fn skip_zero_permissions(mut self, enabled: bool) -> Self {
        self.skip_zero_permissions = enabled;
        self
    }

    // ── Internal helpers (used by overlay.rs) ────────────────────────────────

    /// Returns `true` when the given **relative** path should be excluded from
    /// the commit based on the current filter configuration.
    ///
    /// This is the central decision function invoked by both `commit_copy_phase`
    /// and `copy_tree` for every directory entry they visit.
    ///
    /// # Arguments
    /// * `rel`       – Path relative to the overlay root (e.g. `"dev/null"`).
    ///                 Must not contain a leading `/`.
    /// * `abs_upper` – Absolute physical path of the entry in the upper layer,
    ///                 used only when the zero-permission check is active.
    ///
    /// # Returns
    /// * `true` if the entry should be skipped.
    /// * `false` if the entry should be committed normally.
    pub(crate) fn should_skip(&self, rel: &Path, abs_upper: &Path) -> bool {
        // ── 1. Exact filename check ──────────────────────────────────────────
        if let Some(name) = rel.file_name() {
            if self.skip_files.contains(name.to_string_lossy().as_ref()) {
                return true;
            }
        }

        // ── 2. Directory prefix check ────────────────────────────────────────
        //
        // `strip_prefix` performs component-level matching, so "dev" correctly
        // matches "dev/null" but not "devices/something".
        for skipped in &self.skip_dirs {
            if rel == skipped || rel.strip_prefix(skipped).is_ok() {
                return true;
            }
        }

        // ── 3. Zero-permission check ─────────────────────────────────────────
        if self.skip_zero_permissions {
            if let Ok(meta) = fs::symlink_metadata(abs_upper) {
                if !meta.file_type().is_symlink() {
                    let mode = meta.permissions().mode() & 0o777;
                    if mode == 0 {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// Returns `true` if `rel` is, or is a descendant of, a directory listed
    /// in `skip_dirs`.
    ///
    /// Used by traversal loops to avoid calling `read_dir` on directories that
    /// would be excluded anyway, saving unnecessary syscalls.
    pub(crate) fn is_skipped_dir(&self, rel: &Path) -> bool {
        self.skip_dirs
            .iter()
            .any(|d| rel == d || rel.strip_prefix(d).is_ok())
    }
}
