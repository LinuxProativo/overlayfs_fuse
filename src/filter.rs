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
//! - **Zero-permission files** (mode `0o000`) – these are almost always device
//!   stubs or deliberately inaccessible entries that should not propagate.
//! - **Empty files inside specific directories** – zero-byte regular files that
//!   appear in certain paths (e.g. `/var/cache`, `/var/log`) and are sandbox
//!   artifacts with no meaningful content to persist.
//! - **Custom paths / filenames** – caller-supplied lists for project-specific
//!   exclusions.
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::path::PathBuf;
//! use overlayfs_fuse::OverlayFS;
//! use overlayfs_fuse::CommitFilter;
//!
//! let mut overlay = OverlayFS::new(PathBuf::from("test"));
//!
//! let filter = CommitFilter::rootfs()
//!     .skip_dir("/opt/scratch")
//!     .skip_dirs(["/var/tmp", "/var/run"])
//!     .skip_file("lost+found")
//!     .skip_files(["__pycache__", ".DS_Store"])
//!     .skip_empty_files_in("/var/cache")
//!     .skip_empty_files_in("/var/log")
//!     .skip_zero_permissions(true)
//!     .skip_regex(r".*\.tmp$")
//!     .skip_glob("**/*.bak");
//!
//! overlay.set_commit_filter(filter);
//! ```

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use regex::Regex;
use glob::Pattern;

/// Sanitizes a path by removing the leading root slash if present.
///
/// # Arguments
/// * `p` - The path to be normalized.
///
/// # Returns
/// * A path slice without the leading `/`.
fn normalise_dir(p: &Path) -> &Path {
    p.strip_prefix("/").unwrap_or(p)
}

/// Checks if a given path is equal to or starts with a specific prefix.
///
/// # Arguments
/// * `rel` - The relative path to check.
/// * `prefix` - The prefix to look for.
///
/// # Returns
/// * `true` if `rel` is within or equal to `prefix`.
fn has_prefix(rel: &Path, prefix: &Path) -> bool {
    rel == prefix || rel.strip_prefix(prefix).is_ok()
}

/// Controls which paths are excluded when committing upper-layer changes into
/// the lower layer.
///
/// An entry is skipped when **any** of the following conditions match:
///
/// 1. Its exact filename appears in the `skip_files` set (checked at every depth).
/// 2. Its root-relative path equals, or is a descendant of, a path in `skip_dirs`.
/// 3. Its Unix permission bits are `0o000` and `skip_zero_permissions` is enabled
///    (symlinks are exempt; Linux always reports `0o777` for them).
/// 4. It is a zero-byte regular file whose root-relative parent directory is
///    listed in `skip_empty_files_in`.
/// 5. Its root-relative path matches a provided regular expression.
/// 6. Its root-relative path matches a provided glob pattern.
///
/// All checks operate on the **relative** path as it appears inside the mounted
/// rootfs, so rules can be written in rootfs terms (`"dev"`, `"proc"`) without
/// knowing the physical host location of the upper directory.
#[derive(Debug, Clone, Default)]
pub struct CommitFilter {
    /// Root-relative paths that MUST be committed even if other rules match.
    pub force_files: HashSet<PathBuf>,
    /// Root-relative directory paths whose entire subtree is excluded.
    pub skip_dirs: HashSet<PathBuf>,
    /// Exact filenames (bare name only, no directory component) that are
    /// excluded regardless of where they appear in the tree.
    pub skip_files: HashSet<String>,
    /// Root-relative directories inside which zero-byte regular files are excluded.
    pub skip_empty_files_in: HashSet<PathBuf>,
    /// When `true`, any non-symlink entry with Unix mode `0o000` is excluded.
    pub skip_zero_permissions: bool,
    /// Regular expressions matched against the full root-relative path.
    pub skip_regexes: Vec<Regex>,
    /// Glob patterns matched against the full root-relative path.
    pub skip_globs: Vec<Pattern>,
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
    /// | Path | Reason |
    /// |------|--------|
    /// | `/boot` | Bootloader files and kernels; should not be modified by the overlay. |
    /// | `/dev` | Character/block devices managed by the kernel. |
    /// | `/lost+found` | Filesystem recovery directory. |
    /// | `/proc` | Virtual procfs; kernel-generated. |
    /// | `/sys` | sysfs; kernel ABI. |
    /// | `/run` | Runtime state; meaningless after the session ends. |
    /// | `/tmp` | Temporary files. |
    /// | `/mnt` | Generic mount target. |
    /// | `/media` | Removable-media mount points. |
    ///
    /// Additionally, `skip_zero_permissions` is enabled because rootfs overlays
    /// routinely produce `0o000` stubs for `null`, `zero`, `random`, etc.
    pub fn rootfs() -> Self {
        const ROOTFS_SKIP_DIRS: &[&str] =
            &["boot", "dev", "lost+found", "media", "mnt", "proc", "run", "sys", "tmp"];

        let mut filter = Self::new();
        filter.skip_zero_permissions = true;

        for dir in ROOTFS_SKIP_DIRS {
            filter.skip_dirs.insert(PathBuf::from(dir));
        }
        filter
    }

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
    /// * `Self` with the new exclusion added (a builder pattern).
    pub fn skip_dir(mut self, path: impl AsRef<Path>) -> Self {
        self.skip_dirs
            .insert(normalise_dir(path.as_ref()).to_path_buf());
        self
    }

    /// Excludes every directory in `paths` from the commit.
    ///
    /// Accepts any iterator whose items convert to `Path`, so you can pass
    /// arrays, slices, or any other iterator directly.
    ///
    /// # Arguments
    /// * `paths` - An iterator of paths to exclude.
    ///
    /// # Returns
    /// * `Self` with the directory exclusions added.
    pub fn skip_dirs<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item=P>,
        P: AsRef<Path>,
    {
        for p in paths {
            self.skip_dirs
                .insert(normalise_dir(p.as_ref()).to_path_buf());
        }
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
    /// * `Self` with the filename exclusion added (a builder pattern).
    pub fn skip_file(mut self, name: impl Into<String>) -> Self {
        self.skip_files.insert(name.into());
        self
    }

    /// Excludes every filename in `names` from the commit.
    ///
    /// Accepts any iterator whose items convert to `String`.
    ///
    /// # Arguments
    /// * `names` - An iterator of filenames to exclude.
    ///
    /// # Returns
    /// * `Self` with the filename exclusions added.
    pub fn skip_files<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item=S>,
        S: Into<String>,
    {
        for n in names {
            self.skip_files.insert(n.into());
        }
        self
    }

    /// Registers a root-relative path to be forcibly committed, bypassing standard skip rules.
    ///
    /// # Arguments
    /// * `path` – The root-relative path to the file or directory to be forced.
    ///
    /// # Returns
    /// * `Self` with the forced path added (builder pattern).
    pub fn force_file(mut self, path: impl AsRef<Path>) -> Self {
        self.force_files.insert(normalise_dir(path.as_ref()).to_path_buf());
        self
    }

    /// Registers multiple root-relative paths to be forcibly committed.
    ///
    /// # Arguments
    /// * `paths` – An iterator of paths to be forced.
    ///
    /// # Returns
    /// * `Self` with the forced paths added.
    pub fn force_files<I, P>(mut self, paths: I) -> Self
    where
        I: IntoIterator<Item=P>,
        P: AsRef<Path>,
    {
        for p in paths {
            self.force_files.insert(normalise_dir(p.as_ref()).to_path_buf());
        }
        self
    }

    /// Excludes zero-byte regular files found inside `dir` (at any depth).
    ///
    /// Unlike [`skip_dir`], the directory itself **is** committed — only
    /// empty regular files within it are dropped.  This is useful for cache or
    /// log directories where the sandbox creates placeholder files that have no
    /// meaningful content to persist into the lower layer.
    ///
    /// A leading `/` is stripped, so `"/var/cache"` and `"var/cache"` are
    /// equivalent.
    ///
    /// # Arguments
    /// * `dir` - The directory path inside which empty files will be ignored.
    ///
    /// # Returns
    /// * `Self` with the rule added.
    pub fn skip_empty_files_in(mut self, dir: impl AsRef<Path>) -> Self {
        self.skip_empty_files_in
            .insert(normalise_dir(dir.as_ref()).to_path_buf());
        self
    }

    /// Excludes zero-byte regular files found inside every directory in `dirs`.
    /// Batch variant of [`skip_empty_files_in`].
    ///
    /// # Arguments
    /// * `dirs` - An iterator of directory paths.
    ///
    /// # Returns
    /// * `Self` with the rules added.
    pub fn skip_empty_files_in_dirs<I, P>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item=P>,
        P: AsRef<Path>,
    {
        for d in dirs {
            self.skip_empty_files_in
                .insert(normalise_dir(d.as_ref()).to_path_buf());
        }
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

    /// Adds a regular expression pattern that should be excluded from the commit.
    ///
    /// The regex is matched against the full root-relative path of each entry.
    /// If the pattern fails to compile, it is silently ignored.
    ///
    /// # Arguments
    /// * `pattern` – A valid regex string (e.g. `r".*\.tmp$"`).
    ///
    /// # Returns
    /// * `Self` with the regex exclusion added (a builder pattern).
    pub fn skip_regex(mut self, pattern: &str) -> Self {
        if let Ok(re) = Regex::new(pattern) {
            self.skip_regexes.push(re);
        }
        self
    }

    /// Adds a glob pattern that should be excluded from the commit.
    ///
    /// The glob is matched against the full root-relative path. This is useful
    /// for gitignore-style exclusions like `**/*.bak` or `build/*`.
    /// If the pattern fails to compile, it is silently ignored.
    ///
    /// # Arguments
    /// * `pattern` – A valid glob string (e.g. `"**/target/*"`).
    ///
    /// # Returns
    /// * `Self` with the glob exclusion added (a builder pattern).
    pub fn skip_glob(mut self, pattern: &str) -> Self {
        if let Ok(glob) = Pattern::new(pattern) {
            self.skip_globs.push(glob);
        }
        self
    }

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
        let rel_str = rel.to_string_lossy();

        for re in &self.skip_regexes {
            if re.is_match(&rel_str) {
                return true;
            }
        }

        for glob in &self.skip_globs {
            if glob.matches(&rel_str) {
                return true;
            }
        }

        if self.skip_files.contains(rel_str.as_ref()) {
            return true;
        }

        for skipped in &self.skip_dirs {
            if has_prefix(rel, skipped) {
                return true;
            }
        }

        if self.skip_zero_permissions || !self.skip_empty_files_in.is_empty() {
            if let Ok(meta) = fs::symlink_metadata(abs_upper) {
                let ft = meta.file_type();

                if self.skip_zero_permissions && !ft.is_symlink() {
                    if meta.permissions().mode() & 0o777 == 0 {
                        return true;
                    }
                }

                if ft.is_file() && meta.len() == 0 && !self.skip_empty_files_in.is_empty() {
                    let mut ancestor = rel.parent();
                    while let Some(dir) = ancestor {
                        if self.skip_empty_files_in.contains(dir) {
                            return true;
                        }
                        ancestor = dir.parent();
                    }
                }
            }
        }

        false
    }

    /// Checks if a specific path is marked for forced synchronization.
    ///
    /// # Arguments
    /// * `rel` – The relative path to check.
    ///
    /// # Returns
    /// * `true` if the path is in the forced files set, `false` otherwise.
    pub(crate) fn is_forced(&self, rel: &Path) -> bool {
        self.force_files.contains(rel)
    }

    /// Checks if a specific directory path is marked as skipped.
    ///
    /// # Arguments
    /// * `rel` - The relative path of the directory.
    ///
    /// # Returns
    /// * `true` if the directory or any of its parent directories are in `skip_dirs`.
    pub(crate) fn is_skipped_dir(&self, rel: &Path) -> bool {
        self.skip_dirs.iter().any(|d| has_prefix(rel, d))
    }

    /// Excludes the `/home` directory and all of its descendants from the commit.
    ///
    /// # Returns
    /// * `Self` with the home directory exclusion added (a builder pattern).
    pub fn skip_homedir(mut self) -> Self {
        self.skip_dirs.insert(PathBuf::from("home"));
        self
    }

    /// Excludes the `/root` directory (the superuser's home) and all of its descendants.
    ///
    /// # Returns
    /// * `Self` with the root directory exclusion added (a builder pattern).
    pub fn skip_rootdir(mut self) -> Self {
        self.skip_dirs.insert(PathBuf::from("root"));
        self
    }
}
