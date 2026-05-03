//! File path management for OverlayFS layers.
//!
//! This module defines the core structure for tracking the physical locations
//! of the different filesystem layers (lower, upper, and work directories).

use std::env;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Represents the physical paths required to operate a FUSE-based overlay.
#[derive(Clone)]
pub struct OverlayFiles {
    /// The read-only base layer.
    pub lower: PathBuf,
    /// The read-write layer where changes are stored.
    pub upper: PathBuf,
    /// The internal working directory used for atomic operations.
    pub mount_point: PathBuf,
}

impl OverlayFiles {
    /// Initializes a new `OverlayFiles` structure based on a root lower path.
    ///
    /// This constructor automatically derives the `upper` and `work` directory
    /// paths by appending extensions to the provided `lower` path.
    ///
    /// # Arguments
    /// * `lower` - The `PathBuf` pointing to the base read-only directory.
    ///
    /// # Returns
    /// * A new `OverlayFiles` instance with derived paths for all layers.
    pub fn new(lower: PathBuf) -> Self {
        let name = lower.file_name().unwrap_or_default().to_string_lossy();
        let parent = lower.parent().unwrap_or(Path::new("."));

        let pid = std::process::id();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();

        let session_id = format!("{:x}", pid ^ now.subsec_micros());
        let full_ts = now.as_nanos();

        let mount_name = format!("mount_{}_{}_{}", name, session_id, full_ts);
        Self {
            lower: lower.clone(),
            upper: parent.join(format!("{}_upper", name)),
            mount_point: env::temp_dir().join(mount_name),
        }
    }

    /// Redirects the mount point to the user's home cache directory if available.
    ///
    /// If $HOME is found, it moves the mount point to ~/.cache/mount_name.
    /// If $HOME is not found, it keeps the current mount point (which defaults to /tmp).
    pub fn mountpoint_as_home(&mut self) {
        let name = self.lower.file_name().unwrap_or_default().to_string_lossy();

        let home_cache = env::var("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .ok()
            .or_else(|| env::var("HOME").map(|h| PathBuf::from(h).join(".cache")).ok());

        if let Some(cache_path) = home_cache {
            let _ = std::fs::create_dir_all(&cache_path);
            let new_mount = cache_path.join(format!("mount_{}", name));
            self.mount_point = new_mount;
        }
    }
}
