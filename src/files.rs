//! File path management for OverlayFS layers.
//!
//! This module defines the core structure for tracking the physical locations
//! of the different filesystem layers (lower, upper, and work directories).

use std::path::{Path, PathBuf};

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

        Self {
            lower: lower.clone(),
            upper: parent.join(format!("{}.upper", name)),
            mount_point:  parent.join(format!("{}.mountpoint",  name)),
        }
    }
}
